import { SlashCommandBuilder, PermissionFlagsBits, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('levelconfig')
  .setDescription('Configure the leveling & XP system.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)

  // ── Enable / Disable ─────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('enable').setDescription('Enable the leveling system.')
  )
  .addSubcommand((s) =>
    s.setName('disable').setDescription('Disable the leveling system.')
  )

  // ── XP settings ──────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('rate')
      .setDescription('Set the base XP earned per message (actual value varies ±20%).')
      .addIntegerOption((o) =>
        o.setName('amount').setDescription('Base XP per message (1–100)').setRequired(true).setMinValue(1).setMaxValue(100)
      )
  )
  .addSubcommand((s) =>
    s.setName('cooldown')
      .setDescription('Set the XP cooldown in seconds (prevents spam).')
      .addIntegerOption((o) =>
        o.setName('seconds').setDescription('Cooldown in seconds (5–3600)').setRequired(true).setMinValue(5).setMaxValue(3600)
      )
  )
  .addSubcommand((s) =>
    s.setName('multiplier')
      .setDescription('Set the global XP multiplier (e.g. 2.0 = double XP).')
      .addNumberOption((o) =>
        o.setName('value').setDescription('Multiplier (0.1–10.0)').setRequired(true).setMinValue(0.1).setMaxValue(10)
      )
  )

  // ── Level-up notifications ────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('channel')
      .setDescription('Set the channel for level-up announcements (or clear it to use the message channel).')
      .addChannelOption((o) =>
        o.setName('channel').setDescription('Announcement channel (omit to clear)')
      )
  )
  .addSubcommand((s) =>
    s.setName('message')
      .setDescription('Set the level-up message. Supports {mention}, {level}, {user}, {server}.')
      .addStringOption((o) =>
        o.setName('text').setDescription('Level-up message text').setRequired(true).setMaxLength(500)
      )
  )

  // ── Voice XP ─────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('voicexp')
      .setDescription('Toggle voice XP and set the rate per minute.')
      .addBooleanOption((o) =>
        o.setName('enabled').setDescription('Enable voice XP?').setRequired(true)
      )
      .addIntegerOption((o) =>
        o.setName('rate').setDescription('XP per minute in voice (1–50)').setMinValue(1).setMaxValue(50)
      )
  )

  // ── Exemptions ────────────────────────────────────────────────────────────
  .addSubcommandGroup((g) =>
    g.setName('noxp')
      .setDescription('Manage roles/channels that earn no XP.')
      .addSubcommand((s) =>
        s.setName('role')
          .setDescription('Add or remove a no-XP role.')
          .addRoleOption((o) => o.setName('role').setDescription('Role').setRequired(true))
          .addBooleanOption((o) => o.setName('remove').setDescription('Remove from the list?'))
      )
      .addSubcommand((s) =>
        s.setName('channel')
          .setDescription('Add or remove a no-XP channel.')
          .addChannelOption((o) => o.setName('channel').setDescription('Channel').setRequired(true))
          .addBooleanOption((o) => o.setName('remove').setDescription('Remove from the list?'))
      )
  )

  // ── Role rewards ─────────────────────────────────────────────────────────
  .addSubcommandGroup((g) =>
    g.setName('role')
      .setDescription('Manage level-up role rewards.')
      .addSubcommand((s) =>
        s.setName('add')
          .setDescription('Award a role when a member reaches a specific level.')
          .addIntegerOption((o) =>
            o.setName('level').setDescription('Level to trigger the reward').setRequired(true).setMinValue(1).setMaxValue(999)
          )
          .addRoleOption((o) => o.setName('role').setDescription('Role to award').setRequired(true))
      )
      .addSubcommand((s) =>
        s.setName('remove')
          .setDescription('Remove a level role reward.')
          .addIntegerOption((o) =>
            o.setName('level').setDescription('Level to remove the reward from').setRequired(true).setMinValue(1)
          )
      )
      .addSubcommand((s) =>
        s.setName('list').setDescription('List all level role rewards.')
      )
      .addSubcommand((s) =>
        s.setName('stack')
          .setDescription('Toggle whether lower-tier role rewards are kept when earning a higher one.')
          .addBooleanOption((o) =>
            o.setName('enabled').setDescription('true = keep all (stack), false = remove lower tiers').setRequired(true)
          )
      )
  )

  // ── Overview ─────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('status').setDescription('Show current leveling configuration.')
  );

// ── Helper ────────────────────────────────────────────────────────────────────

async function bust(guildId) {
  await cache.del(`guild:${guildId}:settings`);
}

async function upsertSettings(guildId, data) {
  const settings = await prisma.guildSettings.upsert({
    where:  { guildId },
    update: data,
    create: { guildId, ...data },
  });
  await bust(guildId);
  return settings;
}

// ── Execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  await interaction.deferReply();

  const sub      = interaction.options.getSubcommand();
  const group    = interaction.options.getSubcommandGroup(false);
  const guild    = interaction.guild;
  const guildId  = guild.id;

  // ── Enable / Disable ───────────────────────────────────────────────────────
  if (sub === 'enable') {
    await upsertSettings(guildId, { xpEnabled: true });
    return interaction.editReply({ embeds: [successEmbed('Leveling Enabled', 'Members will now earn XP for chatting.')] });
  }

  if (sub === 'disable') {
    await upsertSettings(guildId, { xpEnabled: false });
    return interaction.editReply({ embeds: [successEmbed('Leveling Disabled', 'XP is no longer granted. Existing data is preserved.')] });
  }

  // ── Rate ───────────────────────────────────────────────────────────────────
  if (sub === 'rate') {
    const amount = interaction.options.getInteger('amount', true);
    await upsertSettings(guildId, { xpRate: amount });
    return interaction.editReply({
      embeds: [successEmbed('XP Rate Updated', `Base XP per message set to **${amount}** (actual: ${Math.floor(amount * 0.8)}–${Math.floor(amount * 1.2)} after variance).`)],
    });
  }

  // ── Cooldown ───────────────────────────────────────────────────────────────
  if (sub === 'cooldown') {
    const seconds = interaction.options.getInteger('seconds', true);
    await upsertSettings(guildId, { xpCooldown: seconds });
    return interaction.editReply({
      embeds: [successEmbed('Cooldown Updated', `XP cooldown set to **${seconds}s** per user.`)],
    });
  }

  // ── Multiplier ─────────────────────────────────────────────────────────────
  if (sub === 'multiplier') {
    const value = interaction.options.getNumber('value', true);
    await upsertSettings(guildId, { xpMultiplier: value });
    return interaction.editReply({
      embeds: [successEmbed('Multiplier Updated', `Global XP multiplier set to **×${value}**.`)],
    });
  }

  // ── Channel ────────────────────────────────────────────────────────────────
  if (sub === 'channel') {
    const ch = interaction.options.getChannel('channel');
    await upsertSettings(guildId, { levelUpChannelId: ch?.id ?? null });
    return interaction.editReply({
      embeds: [successEmbed('Channel Updated', ch
        ? `Level-up messages will be sent to <#${ch.id}>.`
        : 'Level-up messages will be sent in the channel where the level-up occurred.'
      )],
    });
  }

  // ── Message ────────────────────────────────────────────────────────────────
  if (sub === 'message') {
    const text = interaction.options.getString('text', true);
    await upsertSettings(guildId, { levelUpMessage: text });
    return interaction.editReply({
      embeds: [successEmbed('Message Updated', `Level-up message set to:\n> ${text}`)],
    });
  }

  // ── Voice XP ───────────────────────────────────────────────────────────────
  if (sub === 'voicexp') {
    const enabled = interaction.options.getBoolean('enabled', true);
    const rate    = interaction.options.getInteger('rate');
    const update  = { voiceXpEnabled: enabled };
    if (rate !== null) update.voiceXpRate = rate;
    await upsertSettings(guildId, update);
    return interaction.editReply({
      embeds: [successEmbed('Voice XP Updated',
        enabled
          ? `Voice XP enabled at **${rate ?? 5} XP/minute**.`
          : 'Voice XP disabled.'
      )],
    });
  }

  // ── No-XP role ─────────────────────────────────────────────────────────────
  if (group === 'noxp' && sub === 'role') {
    const role    = interaction.options.getRole('role', true);
    const remove  = interaction.options.getBoolean('remove') ?? false;
    const current = await prisma.guildSettings.findUnique({ where: { guildId }, select: { noXpRoleIds: true } });
    const list    = current?.noXpRoleIds ?? [];

    const updated = remove
      ? list.filter((id) => id !== role.id)
      : list.includes(role.id) ? list : [...list, role.id];

    await upsertSettings(guildId, { noXpRoleIds: updated });
    return interaction.editReply({
      embeds: [successEmbed('No-XP Roles', remove
        ? `<@&${role.id}> removed from the no-XP list.`
        : `<@&${role.id}> added to the no-XP list.`
      )],
    });
  }

  // ── No-XP channel ──────────────────────────────────────────────────────────
  if (group === 'noxp' && sub === 'channel') {
    const ch     = interaction.options.getChannel('channel', true);
    const remove = interaction.options.getBoolean('remove') ?? false;
    const current = await prisma.guildSettings.findUnique({ where: { guildId }, select: { noXpChannelIds: true } });
    const list    = current?.noXpChannelIds ?? [];

    const updated = remove
      ? list.filter((id) => id !== ch.id)
      : list.includes(ch.id) ? list : [...list, ch.id];

    await upsertSettings(guildId, { noXpChannelIds: updated });
    return interaction.editReply({
      embeds: [successEmbed('No-XP Channels', remove
        ? `<#${ch.id}> removed from the no-XP list.`
        : `<#${ch.id}> added to the no-XP list.`
      )],
    });
  }

  // ── Role rewards: add ──────────────────────────────────────────────────────
  if (group === 'role' && sub === 'add') {
    const level  = interaction.options.getInteger('level', true);
    const role   = interaction.options.getRole('role', true);

    if (role.managed || role.id === guild.id) {
      return interaction.editReply({ embeds: [errorEmbed('Invalid Role', 'Managed roles and @everyone cannot be used.')] });
    }

    await prisma.levelRole.upsert({
      where:  { guildId_level: { guildId, level } },
      update: { roleId: role.id },
      create: { guildId, level, roleId: role.id },
    });

    return interaction.editReply({
      embeds: [successEmbed('Role Reward Added', `Members who reach level **${level}** will be awarded <@&${role.id}>.`)],
    });
  }

  // ── Role rewards: remove ────────────────────────────────────────────────────
  if (group === 'role' && sub === 'remove') {
    const level = interaction.options.getInteger('level', true);
    const deleted = await prisma.levelRole.deleteMany({ where: { guildId, level } });

    if (deleted.count === 0) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', `No role reward configured for level ${level}.`)] });
    }

    return interaction.editReply({
      embeds: [successEmbed('Role Reward Removed', `Role reward for level **${level}** removed.`)],
    });
  }

  // ── Role rewards: list ─────────────────────────────────────────────────────
  if (group === 'role' && sub === 'list') {
    const roles = await prisma.levelRole.findMany({
      where:   { guildId },
      orderBy: { level: 'asc' },
    });

    if (roles.length === 0) {
      return interaction.editReply({ embeds: [infoEmbed('No Role Rewards', 'No level role rewards configured. Add one with `/levelconfig role add`.')] });
    }

    const embed = new EmbedBuilder()
      .setColor(COLORS.INFO)
      .setTitle('Level Role Rewards')
      .setDescription(roles.map((r) => `**Level ${r.level}** → <@&${r.roleId}>`).join('\n'))
      .setTimestamp();

    return interaction.editReply({ embeds: [embed] });
  }

  // ── Role rewards: stack ────────────────────────────────────────────────────
  if (group === 'role' && sub === 'stack') {
    const enabled = interaction.options.getBoolean('enabled', true);
    await upsertSettings(guildId, { levelRolesStack: enabled });
    return interaction.editReply({
      embeds: [successEmbed('Stack Mode Updated', enabled
        ? 'Role rewards will **stack** — members keep all earned roles.'
        : 'Replace mode enabled — only the highest-tier reward role is kept.'
      )],
    });
  }

  // ── Status ─────────────────────────────────────────────────────────────────
  if (sub === 'status') {
    const s = await prisma.guildSettings.findUnique({ where: { guildId } });
    const levelRolesCount = await prisma.levelRole.count({ where: { guildId } });

    const embed = new EmbedBuilder()
      .setColor(s?.xpEnabled ? COLORS.SUCCESS : COLORS.NEUTRAL)
      .setTitle('⚡ Leveling Configuration')
      .addFields(
        { name: 'Status',          value: s?.xpEnabled ? '✅ Enabled' : '❌ Disabled',         inline: true },
        { name: 'XP Rate',         value: `${s?.xpRate ?? 15} (±20%)`,                          inline: true },
        { name: 'Cooldown',        value: `${s?.xpCooldown ?? 60}s`,                            inline: true },
        { name: 'Multiplier',      value: `×${s?.xpMultiplier ?? 1.0}`,                         inline: true },
        { name: 'Level-up Channel', value: s?.levelUpChannelId ? `<#${s.levelUpChannelId}>` : 'Same channel', inline: true },
        { name: 'Voice XP',        value: s?.voiceXpEnabled ? `✅ ${s.voiceXpRate ?? 5}/min` : '❌ Off', inline: true },
        { name: 'Role Stack Mode', value: (s?.levelRolesStack ?? true) ? 'Stack' : 'Replace',   inline: true },
        { name: 'Role Rewards',    value: `${levelRolesCount} configured`,                       inline: true },
        { name: 'No-XP Roles',     value: s?.noXpRoleIds?.length ? s.noXpRoleIds.map((id) => `<@&${id}>`).join(', ') : 'None', inline: false },
        { name: 'No-XP Channels',  value: s?.noXpChannelIds?.length ? s.noXpChannelIds.map((id) => `<#${id}>`).join(', ') : 'None', inline: false },
      )
      .setTimestamp();

    if (s?.levelUpMessage) {
      embed.addFields({ name: 'Level-up Message', value: s.levelUpMessage, inline: false });
    }

    return interaction.editReply({ embeds: [embed] });
  }
}
