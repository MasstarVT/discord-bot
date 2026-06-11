import {
  SlashCommandBuilder,
  PermissionFlagsBits,
  ButtonStyle,
  EmbedBuilder,
} from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { buildPanelPayload, syncPanel } from '../../services/reactionRoleService.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.ADMIN;

// ── ButtonStyle name → value map ───────────────────────────────────────────────
const STYLE_MAP = { Primary: 1, Secondary: 2, Success: 3, Danger: 4 };

export const data = new SlashCommandBuilder()
  .setName('reactionrole')
  .setDescription('Manage reaction role panels.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageRoles)

  // ── Create ─────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('create')
      .setDescription('Create a new reaction role panel and post it to a channel.')
      .addStringOption((o) => o.setName('title').setDescription('Panel title').setRequired(true))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel to post the panel in').setRequired(true))
      .addStringOption((o) =>
        o.setName('type')
          .setDescription('Role assignment logic')
          .setRequired(true)
          .addChoices(
            { name: 'Normal  — toggle on/off',        value: 'NORMAL'  },
            { name: 'Unique  — pick only one',        value: 'UNIQUE'  },
            { name: 'Verify  — add only, no removal', value: 'VERIFY'  },
            { name: 'Reverse — removes the role',     value: 'REVERSE' },
          )
      )
      .addStringOption((o) =>
        o.setName('layout')
          .setDescription('Component layout')
          .setRequired(true)
          .addChoices(
            { name: 'Buttons — up to 25 roles as buttons', value: 'BUTTONS' },
            { name: 'Select  — dropdown menu',             value: 'SELECT'  },
          )
      )
      .addStringOption((o) => o.setName('description').setDescription('Panel description shown under the title'))
      .addIntegerOption((o) =>
        o.setName('max_roles')
          .setDescription('Max roles a member can hold from this panel (NORMAL only)')
          .setMinValue(1).setMaxValue(25)
      )
  )

  // ── Add role ───────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('add')
      .setDescription('Add a role to an existing panel.')
      .addStringOption((o) =>
        o.setName('panel').setDescription('Panel to edit').setRequired(true).setAutocomplete(true)
      )
      .addRoleOption((o) => o.setName('role').setDescription('Role to add').setRequired(true))
      .addStringOption((o) => o.setName('label').setDescription('Button/option label (max 80 chars)').setRequired(true))
      .addStringOption((o) => o.setName('emoji').setDescription('Emoji for the button/option (Unicode or :name:)'))
      .addStringOption((o) =>
        o.setName('style')
          .setDescription('Button colour (BUTTONS layout only)')
          .addChoices(
            { name: 'Primary   (blue)',  value: 'Primary'   },
            { name: 'Secondary (grey)',  value: 'Secondary' },
            { name: 'Success   (green)', value: 'Success'   },
            { name: 'Danger    (red)',   value: 'Danger'    },
          )
      )
  )

  // ── Remove role ────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('remove')
      .setDescription('Remove a role from a panel.')
      .addStringOption((o) =>
        o.setName('panel').setDescription('Panel to edit').setRequired(true).setAutocomplete(true)
      )
      .addRoleOption((o) => o.setName('role').setDescription('Role to remove').setRequired(true))
  )

  // ── Delete panel ───────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('delete')
      .setDescription('Delete a reaction role panel and its Discord message.')
      .addStringOption((o) =>
        o.setName('panel').setDescription('Panel to delete').setRequired(true).setAutocomplete(true)
      )
  )

  // ── List ───────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('list')
      .setDescription('List all reaction role panels in this server.')
  );

// ── Autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  const focused = interaction.options.getFocused().toLowerCase();

  const groups = await prisma.reactionRoleGroup.findMany({
    where: { guildId: interaction.guildId },
    include: { items: { select: { id: true } } },
    take: 25,
  });

  const filtered = focused
    ? groups.filter((g) => g.title.toLowerCase().includes(focused) || g.messageId.includes(focused))
    : groups;

  await interaction.respond(
    filtered.slice(0, 25).map((g) => ({
      name:  `${g.title} (${g.type} · ${g.items.length} roles)`,
      value: g.id,
    }))
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async function invalidateSettings(guildId) {
  await cache.del(`guild:${guildId}:settings`);
}

// ── Main execute ──────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  await interaction.deferReply();

  const sub   = interaction.options.getSubcommand();
  const guild = interaction.guild;

  // ── Create ─────────────────────────────────────────────────────────────────
  if (sub === 'create') {
    const title       = interaction.options.getString('title', true);
    const channel     = interaction.options.getChannel('channel', true);
    const type        = interaction.options.getString('type', true);
    const layout      = interaction.options.getString('layout', true);
    const description = interaction.options.getString('description');
    const maxRoles    = interaction.options.getInteger('max_roles');

    if (!channel.isTextBased()) {
      return interaction.editReply({ embeds: [errorEmbed('Invalid Channel', 'Please select a text channel.')] });
    }

    // Build a temporary empty group to get an initial payload
    const tempGroup = { id: '__pending__', title, description, type, layout, maxRoles, items: [] };
    const { embeds, components } = buildPanelPayload(tempGroup);

    // Post the panel message first to get its ID
    let panelMessage;
    try {
      panelMessage = await channel.send({ embeds, components });
    } catch (err) {
      return interaction.editReply({ embeds: [errorEmbed('Send Failed', `Could not post to <#${channel.id}>: ${err.message}`)] });
    }

    // Save to DB
    const group = await prisma.reactionRoleGroup.create({
      data: {
        guildId:     guild.id,
        channelId:   channel.id,
        messageId:   panelMessage.id,
        title,
        description: description ?? null,
        type,
        layout,
        maxRoles:    maxRoles ?? null,
      },
    });

    // Update settings cache bust
    await invalidateSettings(guild.id);

    return interaction.editReply({
      embeds: [successEmbed(
        'Panel Created',
        [
          `Panel **"${title}"** posted in <#${channel.id}>.`,
          `Add roles with \`/reactionrole add\`.`,
          `\`\`\`\nID: ${group.id}\nMessage: ${panelMessage.id}\n\`\`\``,
        ].join('\n')
      )],
    });
  }

  // ── Add role ───────────────────────────────────────────────────────────────
  if (sub === 'add') {
    const groupId = interaction.options.getString('panel', true);
    const role    = interaction.options.getRole('role', true);
    const label   = interaction.options.getString('label', true).slice(0, 80);
    const emoji   = interaction.options.getString('emoji');
    const styleStr = interaction.options.getString('style') ?? 'Primary';
    const style   = STYLE_MAP[styleStr] ?? ButtonStyle.Primary;

    const group = await prisma.reactionRoleGroup.findUnique({
      where: { id: groupId },
      include: { items: true },
    });
    if (!group || group.guildId !== guild.id) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', 'Panel not found.')] });
    }
    if (group.items.length >= 25) {
      return interaction.editReply({ embeds: [errorEmbed('Panel Full', 'A panel can hold a maximum of 25 roles.')] });
    }
    if (group.items.some((i) => i.roleId === role.id)) {
      return interaction.editReply({ embeds: [errorEmbed('Already Added', `<@&${role.id}> is already in this panel.`)] });
    }
    if (role.managed || role.id === guild.id) {
      return interaction.editReply({ embeds: [errorEmbed('Invalid Role', 'Managed roles and @everyone cannot be used.')] });
    }

    const customId = `rr:${group.id}:${role.id}`;
    await prisma.reactionRoleItem.create({
      data: {
        groupId,
        customId,
        roleId:   role.id,
        label,
        emoji:    emoji ?? null,
        style,
        position: group.items.length,
      },
    });

    await syncPanel(guild, groupId, client);

    return interaction.editReply({
      embeds: [successEmbed('Role Added', `<@&${role.id}> added to panel **"${group.title}"**.`)],
    });
  }

  // ── Remove role ────────────────────────────────────────────────────────────
  if (sub === 'remove') {
    const groupId = interaction.options.getString('panel', true);
    const role    = interaction.options.getRole('role', true);

    const group = await prisma.reactionRoleGroup.findUnique({
      where: { id: groupId },
      include: { items: true },
    });
    if (!group || group.guildId !== guild.id) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', 'Panel not found.')] });
    }

    const item = group.items.find((i) => i.roleId === role.id);
    if (!item) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', `<@&${role.id}> is not in this panel.`)] });
    }

    await prisma.reactionRoleItem.delete({ where: { id: item.id } });

    // Re-position remaining items
    const remaining = group.items.filter((i) => i.id !== item.id).sort((a, b) => a.position - b.position);
    for (let i = 0; i < remaining.length; i++) {
      if (remaining[i].position !== i) {
        await prisma.reactionRoleItem.update({ where: { id: remaining[i].id }, data: { position: i } });
      }
    }

    await syncPanel(guild, groupId, client);

    return interaction.editReply({
      embeds: [successEmbed('Role Removed', `<@&${role.id}> removed from panel **"${group.title}"**.`)],
    });
  }

  // ── Delete panel ───────────────────────────────────────────────────────────
  if (sub === 'delete') {
    const groupId = interaction.options.getString('panel', true);

    const group = await prisma.reactionRoleGroup.findUnique({ where: { id: groupId } });
    if (!group || group.guildId !== guild.id) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', 'Panel not found.')] });
    }

    // Try to delete the Discord message
    const channel = guild.channels.cache.get(group.channelId)
      ?? await guild.channels.fetch(group.channelId).catch(() => null);
    if (channel?.isTextBased()) {
      const msg = await channel.messages.fetch(group.messageId).catch(() => null);
      await msg?.delete().catch(() => null);
    }

    // Cascade-deletes items via FK
    await prisma.reactionRoleGroup.delete({ where: { id: groupId } });

    return interaction.editReply({
      embeds: [successEmbed('Panel Deleted', `Panel **"${group.title}"** and its Discord message have been removed.`)],
    });
  }

  // ── List ───────────────────────────────────────────────────────────────────
  if (sub === 'list') {
    const groups = await prisma.reactionRoleGroup.findMany({
      where:   { guildId: guild.id },
      include: { items: { select: { id: true } } },
      orderBy: { createdAt: 'desc' },
    });

    if (groups.length === 0) {
      return interaction.editReply({
        embeds: [infoEmbed('No Panels', 'No reaction role panels exist in this server.\nCreate one with `/reactionrole create`.')],
      });
    }

    const embed = new EmbedBuilder()
      .setColor(COLORS.INFO)
      .setTitle(`🎭 Reaction Role Panels (${groups.length})`)
      .setTimestamp();

    for (const g of groups.slice(0, 25)) {
      const jumpURL = `https://discord.com/channels/${guild.id}/${g.channelId}/${g.messageId}`;
      embed.addFields({
        name:  g.title,
        value: [
          `**Type:** ${g.type}  **Layout:** ${g.layout}  **Roles:** ${g.items.length}`,
          `**Channel:** <#${g.channelId}>  [Jump](${jumpURL})`,
        ].join('\n'),
      });
    }

    return interaction.editReply({ embeds: [embed] });
  }
}
