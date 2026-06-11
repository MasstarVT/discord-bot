import { SlashCommandBuilder, PermissionFlagsBits, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { errorEmbed } from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import prisma from '../../database/client.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const ACTION_EMOJI = {
  WARN: '⚠️', TIMEOUT: '⏱️', MUTE: '🔇', KICK: '👢',
  SOFTBAN: '🔨', BAN: '🔨', UNBAN: '✅', UNMUTE: '🔊', NOTE: '📝',
};

const CASES_PER_PAGE = 5;

export const data = new SlashCommandBuilder()
  .setName('history')
  .setDescription("View a member's moderation history in this server.")
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to look up').setRequired(true))
  .addStringOption((o) =>
    o.setName('type')
      .setDescription('Filter by action type')
      .addChoices(
        { name: 'Warns',    value: 'WARN'    },
        { name: 'Bans',     value: 'BAN'     },
        { name: 'Kicks',    value: 'KICK'    },
        { name: 'Timeouts', value: 'TIMEOUT' },
        { name: 'Active',   value: 'active'  },
      )
  );

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const target   = interaction.options.getUser('user', true);
  const typeFilter = interaction.options.getString('type');
  const guild    = interaction.guild;

  const where = {
    guildId:      guild.id,
    targetUserId: target.id,
    ...(typeFilter === 'active'
      ? { active: true }
      : typeFilter
        ? { type: typeFilter }
        : {}),
  };

  const infractions = await prisma.infraction.findMany({
    where,
    orderBy: { caseId: 'desc' },
  });

  if (infractions.length === 0) {
    // Check live Discord state so mods aren't misled when records predate the bot
    const [ban, member] = await Promise.all([
      guild.bans.fetch(target.id).catch(() => null),
      guild.members.fetch(target.id).catch(() => null),
    ]);

    const lines = [`No moderation history found for <@${target.id}>` + (typeFilter ? ` (filter: ${typeFilter})` : '') + '.'];

    if (ban) {
      lines.push('');
      lines.push(`⚠️ **This user is currently banned** (pre-bot ban not yet in records).`);
      if (ban.reason) lines.push(`Ban reason on record: \`${ban.reason}\``);
    } else if (member?.isCommunicationDisabled()) {
      lines.push('');
      lines.push(`⚠️ **This member is currently timed out** (pre-bot timeout not in records).`);
      lines.push(`Timeout expires: <t:${Math.floor(member.communicationDisabledUntilTimestamp / 1000)}:R>`);
    }

    await interaction.deleteReply();
    return interaction.followUp({
      embeds: [errorEmbed('No Records', lines.join('\n'))],
    });
  }

  // Count summary
  const counts = infractions.reduce((acc, i) => {
    acc[i.type] = (acc[i.type] ?? 0) + 1;
    return acc;
  }, {});

  const summaryLine = Object.entries(counts)
    .map(([t, n]) => `${ACTION_EMOJI[t] ?? '•'} ${n}× ${t}`)
    .join(' · ');

  // Build pages — CASES_PER_PAGE cases per embed
  const chunks = [];
  for (let i = 0; i < infractions.length; i += CASES_PER_PAGE) {
    chunks.push(infractions.slice(i, i + CASES_PER_PAGE));
  }

  const pages = chunks.map((chunk, idx) => {
    const embed = new EmbedBuilder()
      .setColor(COLORS.INFO)
      .setTitle(`📋 Moderation History — ${target.tag}`)
      .setThumbnail(target.displayAvatarURL())
      .setDescription(summaryLine)
      .setFooter({ text: `${infractions.length} total record(s) · Page ${idx + 1}/${chunks.length}` });

    for (const inf of chunk) {
      const emoji   = ACTION_EMOJI[inf.type] ?? '•';
      const ts      = Math.floor(inf.createdAt.getTime() / 1000);
      const active  = inf.active ? ' 🔴' : '';
      const reason  = inf.reason?.slice(0, 80) ?? 'No reason';
      embed.addFields({
        name:  `${emoji} Case #${inf.caseId} — ${inf.type}${active}`,
        value: `<t:${ts}:D> by <@${inf.moderatorId}>\n> ${reason}`,
      });
    }

    return embed;
  });

  // Single page — no paginator needed
  await interaction.deleteReply();
  if (pages.length === 1) {
    return interaction.followUp({ embeds: [pages[0]] });
  }

  await paginate(interaction, pages, { ephemeral: false });
}
