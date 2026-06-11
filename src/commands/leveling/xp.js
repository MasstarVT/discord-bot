import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { adminAddXp, adminSetXp, adminResetXp, levelFromXp } from '../../services/xpService.js';
import prisma from '../../database/client.js';

export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('xp')
  .setDescription('Admin XP management commands.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)

  .addSubcommand((s) =>
    s.setName('give')
      .setDescription('Give XP to a member.')
      .addUserOption((o) => o.setName('user').setDescription('Target member').setRequired(true))
      .addIntegerOption((o) => o.setName('amount').setDescription('XP to give').setRequired(true).setMinValue(1).setMaxValue(1_000_000))
  )
  .addSubcommand((s) =>
    s.setName('remove')
      .setDescription('Remove XP from a member.')
      .addUserOption((o) => o.setName('user').setDescription('Target member').setRequired(true))
      .addIntegerOption((o) => o.setName('amount').setDescription('XP to remove').setRequired(true).setMinValue(1).setMaxValue(1_000_000))
  )
  .addSubcommand((s) =>
    s.setName('set')
      .setDescription('Set a member\'s total XP to an exact value.')
      .addUserOption((o) => o.setName('user').setDescription('Target member').setRequired(true))
      .addIntegerOption((o) => o.setName('amount').setDescription('New total XP').setRequired(true).setMinValue(0).setMaxValue(10_000_000))
  )
  .addSubcommand((s) =>
    s.setName('reset')
      .setDescription('Reset a member\'s XP and level to zero.')
      .addUserOption((o) => o.setName('user').setDescription('Target member').setRequired(true))
  )
  .addSubcommand((s) =>
    s.setName('view')
      .setDescription('View a member\'s raw XP data.')
      .addUserOption((o) => o.setName('user').setDescription('Target member').setRequired(true))
  );

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const sub     = interaction.options.getSubcommand();
  const target  = interaction.options.getUser('user', true);
  const guildId = interaction.guildId;

  if (sub === 'give') {
    const amount = interaction.options.getInteger('amount', true);
    const record = await adminAddXp(guildId, target.id, amount);
    return interaction.editReply({
      embeds: [successEmbed('XP Given', `Added **${amount.toLocaleString()} XP** to <@${target.id}>.\nNew total: **${record.xp.toLocaleString()} XP** (Level ${levelFromXp(record.xp)})`)],
    });
  }

  if (sub === 'remove') {
    const amount = interaction.options.getInteger('amount', true);
    const record = await adminAddXp(guildId, target.id, -amount);
    return interaction.editReply({
      embeds: [successEmbed('XP Removed', `Removed **${amount.toLocaleString()} XP** from <@${target.id}>.\nNew total: **${record.xp.toLocaleString()} XP** (Level ${levelFromXp(record.xp)})`)],
    });
  }

  if (sub === 'set') {
    const amount = interaction.options.getInteger('amount', true);
    const record = await adminSetXp(guildId, target.id, amount);
    return interaction.editReply({
      embeds: [successEmbed('XP Set', `Set <@${target.id}>'s XP to **${amount.toLocaleString()}** (Level ${levelFromXp(record.xp)}).`)],
    });
  }

  if (sub === 'reset') {
    await adminResetXp(guildId, target.id);
    return interaction.editReply({
      embeds: [successEmbed('XP Reset', `Wiped all XP and level data for <@${target.id}>.`)],
    });
  }

  if (sub === 'view') {
    const record = await prisma.userLevel.findUnique({
      where: { guildId_userId: { guildId, userId: target.id } },
    });
    if (!record) {
      return interaction.editReply({ embeds: [infoEmbed('No Data', `<@${target.id}> has no XP in this server.`)] });
    }
    const rank = await prisma.userLevel.count({ where: { guildId, xp: { gt: record.xp } } }) + 1;
    return interaction.editReply({
      embeds: [{
        color: 0x5865f2,
        title: `XP Data — ${target.username}`,
        fields: [
          { name: 'Level',        value: String(levelFromXp(record.xp)),      inline: true },
          { name: 'Total XP',     value: record.xp.toLocaleString(),           inline: true },
          { name: 'Rank',         value: `#${rank}`,                           inline: true },
          { name: 'Messages',     value: record.messageCount.toLocaleString(), inline: true },
          { name: 'Voice (min)',  value: String(record.voiceMinutes),           inline: true },
          { name: 'Last XP',     value: record.lastXpAt ? `<t:${Math.floor(record.lastXpAt.getTime() / 1000)}:R>` : 'Never', inline: true },
        ],
        thumbnail: { url: target.displayAvatarURL() },
        timestamp: new Date().toISOString(),
      }],
    });
  }
}
