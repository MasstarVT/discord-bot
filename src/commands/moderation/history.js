import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  ACTION_META,
  createInfraction,
  deliverLookup,
  deliverModReply,
  displayType,
} from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { errorEmbed, guildEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { awaitConfirm, isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
/** Clearing warnings is an administrator action; viewing stays with moderators. */
export const subcommandPermissions = { clear: PERMISSION_LEVELS.ADMIN };

const CASES_PER_PAGE = 5;
const MAX_ROWS = 250;
const NO_MENTIONS = { parse: [] };

const TYPE_CHOICES = [
  { name: 'Active only', value: 'active' }, { name: 'Warnings', value: 'WARN' }, { name: 'Timeouts', value: 'TIMEOUT' },
  { name: 'Mutes', value: 'MUTE' }, { name: 'Kicks', value: 'KICK' }, { name: 'Softbans', value: 'SOFTBAN' },
  { name: 'Bans', value: 'BAN' }, { name: 'Unbans', value: 'UNBAN' }, { name: 'Unmutes', value: 'UNMUTE' }, { name: 'Notes', value: 'NOTE' },
];
const TYPE_LABEL = Object.fromEntries(TYPE_CHOICES.map((c) => [c.value, c.name]));

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);

export const data = new SlashCommandBuilder()
  .setName('history')
  .setDescription("View or clear a member's infractions (MEE6 /infractions, /clear-all-infractions).")
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('view').setDescription("List a member's moderation cases (MEE6 /infractions).")
    .addUserOption((o) => o.setName('user').setDescription('Member to look up').setRequired(true))
    .addStringOption((o) => o.setName('type').setDescription('Only show one kind of case').addChoices(...TYPE_CHOICES)))
  .addSubcommand((s) => s.setName('clear').setDescription("Pardon all of a member's active warnings (MEE6 /clear-all-infractions).")
    .addUserOption((o) => o.setName('user').setDescription('Member whose warnings to clear').setRequired(true))
    .addStringOption((o) => reasonOpt(o, 'Why the warnings are cleared')));

const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);
const metaOf = (type) => ACTION_META[type] ?? { emoji: '•', label: String(type) };

/**
 * /history view | clear.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private
  const settings = await getSettingsSafe(interaction.guildId);
  if (interaction.options.getSubcommand() === 'clear') await clearWarnings(interaction, settings);
  else await viewHistory(interaction, settings);
}

// ── view ──────────────────────────────────────────────────────────────────────

/** Live Discord state for a member who has no records at all (hint only; a failed lookup adds no hint). */
async function liveStateHints(guild, userId) {
  const [ban, member] = await Promise.all([
    guild.bans.fetch({ user: userId, force: true, cache: false }).catch(() => null),
    guild.members.fetch(userId).catch(() => null),
  ]);
  const lines = [];
  if (ban) {
    lines.push('⚠️ **This user is currently banned**, but the ban is not in the records yet (it will be imported at the next ban sync).');
    if (ban.reason) lines.push(`Ban reason on record: ${clampText(String(ban.reason).replace(/`/g, "'"), 300)}`);
  } else if (member?.isCommunicationDisabled?.()) {
    lines.push('⚠️ **This member is currently timed out** (not by this bot).');
    lines.push(`Timeout ends: ${discordTimestamp(member.communicationDisabledUntilTimestamp, 'R')}`);
  }
  return lines;
}

async function viewHistory(interaction, settings) {
  const user = interaction.options.getUser('user', true);
  const type = interaction.options.getString('type');
  const guild = interaction.guild;
  const guildId = interaction.guildId;

  const where = { guildId, targetUserId: user.id, ...(type === 'active' ? { active: true } : type ? { type } : {}) };
  const [total, rows, counts, activeWarns] = await Promise.all([
    prisma.infraction.count({ where }),
    prisma.infraction.findMany({ where, orderBy: { caseId: 'desc' }, take: MAX_ROWS }),
    prisma.infraction.groupBy({ by: ['type'], where: { guildId, targetUserId: user.id }, _count: { _all: true } }),
    prisma.infraction.count({ where: { guildId, targetUserId: user.id, type: 'WARN', active: true } }),
  ]);

  if (rows.length === 0) {
    const lines = [`No moderation history for <@${user.id}>${type ? ` (filter: ${TYPE_LABEL[type] ?? type})` : ''}.`];
    // R246: "not in the records yet" is only true when the member has no records at all. A filter that
    // matches nothing, for someone with recorded cases, must not claim a recorded ban is missing.
    if (counts.length === 0) {
      const hints = await liveStateHints(guild, user.id);
      if (hints.length) lines.push('', ...hints);
    }
    await deliverLookup(interaction, settings, { embeds: [infoEmbed('No Records', lines.join('\n'))], allowedMentions: NO_MENTIONS });
    return;
  }

  const summary = clampText([
    [...counts]
      .sort((a, b) => b._count._all - a._count._all || String(a.type).localeCompare(String(b.type)))
      .map((c) => `${metaOf(c.type).emoji} ${c._count._all}× ${metaOf(c.type).label}`)
      .join(' · '),
    `**Active warnings:** ${activeWarns}`,
  ].filter(Boolean).join('\n'), 4096);
  const footer = `${total} record(s)${total > MAX_ROWS ? ` · showing the ${MAX_ROWS} most recent` : ''}`;
  const avatar = typeof user.displayAvatarURL === 'function' ? user.displayAvatarURL() : null;

  const pages = [];
  for (let i = 0; i < rows.length; i += CASES_PER_PAGE) {
    const embed = guildEmbed(settings, clampText(`📋 Infractions | ${user.tag ?? user.username ?? user.id}`, 256), summary, footer);
    if (avatar) embed.setThumbnail(avatar);
    for (const inf of rows.slice(i, i + CASES_PER_PAGE)) {
      const meta = metaOf(displayType(inf));
      const reason = clampText(String(inf.reason ?? 'No reason').replace(/\s*\n\s*/g, ' '), 200);
      embed.addFields({
        name: clampText(`${meta.emoji} Case #${inf.caseId} | ${meta.label}${inf.active ? ' 🔴 active' : ''}`, 256),
        value: clampText(
          `${discordTimestamp(inf.createdAt, 'D')} by <@${inf.moderatorId}>${inf.expiresAt ? ` · ends ${discordTimestamp(inf.expiresAt, 'R')}` : ''}\n> ${reason}`,
          1024,
        ),
      });
    }
    pages.push(embed);
  }

  const publicReply = (settings?.modVisibility ?? 'PUBLIC') === 'PUBLIC';
  try {
    if (publicReply) {
      // R026-R028 / R055: the deferred reply is gone after deleteReply, so the pages go out as follow-ups.
      await interaction.deleteReply().catch(() => {});
      await paginate(interaction, pages, { followUp: true });
    } else {
      await paginate(interaction, pages, { ephemeral: true }); // edits the deferred ephemeral reply
    }
  } catch (err) {
    if (isInteractionGone(err)) return;
    logger.warn(`/history: could not send the pages: ${err?.message ?? err}`);
    const embed = errorEmbed('Action Failed', "I couldn't show the history here. Check my permissions in this channel and try again.");
    if (publicReply) await interaction.followUp({ embeds: [embed], ephemeral: true }).catch(() => {});
    else await interaction.editReply({ embeds: [embed] }).catch(() => {});
  }
}

// ── clear ─────────────────────────────────────────────────────────────────────

async function clearWarnings(interaction, settings) {
  const user = interaction.options.getUser('user', true);
  const reason = interaction.options.getString('reason');
  const guildId = interaction.guildId;
  const activeWarns = { guildId, targetUserId: user.id, type: 'WARN', active: true };

  const n = await prisma.infraction.count({ where: activeWarns });
  if (n === 0) {
    await interaction.editReply({ embeds: [infoEmbed('Nothing To Clear', `<@${user.id}> has no active warnings.`)] });
    return;
  }

  const confirmed = await awaitConfirm(interaction, {
    title: 'Clear warnings?',
    description: `This pardons **${n}** active warning(s) for <@${user.id}>. Their cases stay in the history, and escalation counting starts over.`,
    confirmLabel: 'Clear warnings',
    danger: true,
    ephemeral: true,
  });
  if (!confirmed) return; // awaitConfirm already shows the outcome

  const { count } = await prisma.infraction.updateMany({ where: activeWarns, data: { active: false } });
  const noteReason = clampText(`[Pardon] Cleared ${count} warning(s): ${reason?.trim() || 'No reason provided'}`, 1000);

  // NOTE: the pardon itself is the updateMany above; this only records it as a case (and posts the case log).
  const lines = [
    `**User:** <@${user.id}> \`${tagOf(user)}\``,
    `**Warnings cleared:** ${count}`,
    `**Reason:** ${clampText(reason?.trim() || 'No reason provided', 1000)}`,
  ];
  const warnings = [];
  try {
    const note = await createInfraction(interaction.guild, user, interaction.user, 'NOTE', {
      reason: noteReason,
      silent: true,
      skipEscalation: true,
      client: interaction.client,
    });
    lines.push(`**Case:** #${note.caseId}`);
  } catch (err) {
    logger.error(`/history clear: could not record the pardon note in ${guildId}`, err);
    warnings.push('The warnings were cleared, but the pardon could not be saved as a case.');
  }
  await deliverModReply(interaction, settings, { title: 'Warnings Cleared', lines, warnings });
}
