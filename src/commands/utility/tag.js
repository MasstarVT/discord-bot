import {
  SlashCommandBuilder,
  PermissionFlagsBits,
  EmbedBuilder,
} from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { buildTagPayload } from '../../services/tagService.js';
import { PLACEHOLDER_LIST } from '../../utils/placeholderParser.js';
import { paginate } from '../../utils/paginator.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('tag')
  .setDescription('Create and use custom tag responses.')

  // ── Use ─────────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('use')
      .setDescription('Send a tag\'s saved response.')
      .addStringOption((o) =>
        o.setName('name').setDescription('Tag name').setRequired(true).setAutocomplete(true)
      )
  )

  // ── Create ───────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('create')
      .setDescription('Create a new tag. (Admin/Moderator)')
      .addStringOption((o) => o.setName('name').setDescription('Tag name (lowercase, no spaces)').setRequired(true).setMaxLength(50))
      .addStringOption((o) => o.setName('content').setDescription('Tag response text (supports placeholders)').setRequired(true).setMaxLength(4000))
      .addBooleanOption((o) => o.setName('embed').setDescription('Send as an embed? (default: false)'))
  )

  // ── Edit ─────────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('edit')
      .setDescription('Edit an existing tag. (Admin/Moderator)')
      .addStringOption((o) =>
        o.setName('name').setDescription('Tag to edit').setRequired(true).setAutocomplete(true)
      )
      .addStringOption((o) => o.setName('content').setDescription('New response text').setMaxLength(4000))
      .addBooleanOption((o) => o.setName('embed').setDescription('Send as an embed?'))
  )

  // ── Delete ───────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('delete')
      .setDescription('Delete a tag. (Admin/Moderator)')
      .addStringOption((o) =>
        o.setName('name').setDescription('Tag to delete').setRequired(true).setAutocomplete(true)
      )
  )

  // ── Info ─────────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('info')
      .setDescription('Show details about a tag.')
      .addStringOption((o) =>
        o.setName('name').setDescription('Tag name').setRequired(true).setAutocomplete(true)
      )
  )

  // ── Raw ──────────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('raw')
      .setDescription('Show the raw (unrendered) content of a tag for copying.')
      .addStringOption((o) =>
        o.setName('name').setDescription('Tag name').setRequired(true).setAutocomplete(true)
      )
  )

  // ── List ─────────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('list').setDescription('List all tags in this server.')
  )

  // ── Placeholders help ────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('placeholders').setDescription('Show all available placeholders for tag content.')
  );

// ── Autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  const focused  = interaction.options.getFocused().toLowerCase();
  const guildId  = interaction.guildId;

  const tags = await prisma.customTag.findMany({
    where:   { guildId, ...(focused ? { name: { contains: focused } } : {}) },
    orderBy: { uses: 'desc' },
    take:    25,
    select:  { name: true, uses: true },
  });

  await interaction.respond(
    tags.map((t) => ({ name: `${t.name} (${t.uses} uses)`, value: t.name }))
  );
}

// ── Permission guard for write operations ─────────────────────────────────────

function canWrite(interaction) {
  return (
    interaction.member?.permissions?.has(PermissionFlagsBits.ManageGuild) ||
    interaction.member?.permissions?.has(PermissionFlagsBits.ManageMessages)
  );
}

// ── Execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const sub     = interaction.options.getSubcommand();
  const guildId = interaction.guildId;
  const guild   = interaction.guild;
  const member  = interaction.member;

  // ── Placeholders ─────────────────────────────────────────────────────────────
  if (sub === 'placeholders') {
    return interaction.reply({
      embeds: [
        new EmbedBuilder()
          .setColor(COLORS.INFO)
          .setTitle('Tag Placeholders')
          .setDescription(PLACEHOLDER_LIST)
          .setFooter({ text: 'Use these tokens in tag content — they resolve at use time.' }),
      ],
      ephemeral: true,
    });
  }

  // ── List ─────────────────────────────────────────────────────────────────────
  if (sub === 'list') {
    await interaction.deferReply({ ephemeral: true });

    const tags = await prisma.customTag.findMany({
      where:   { guildId },
      orderBy: { uses: 'desc' },
    });

    if (tags.length === 0) {
      return interaction.editReply({ embeds: [infoEmbed('No Tags', 'No tags exist in this server yet.\nCreate one with `/tag create`.')] });
    }

    const PAGE = 20;
    const pages = [];
    for (let i = 0; i < tags.length; i += PAGE) {
      const chunk = tags.slice(i, i + PAGE);
      pages.push(
        new EmbedBuilder()
          .setColor(COLORS.INFO)
          .setTitle(`📋 Tags (${tags.length} total)`)
          .setDescription(
            chunk.map((t) => `\`${t.name}\` — ${t.uses} use${t.uses !== 1 ? 's' : ''}${t.useEmbed ? ' 🖼️' : ''}`).join('\n')
          )
          .setFooter({ text: `Page ${Math.floor(i / PAGE) + 1} of ${Math.ceil(tags.length / PAGE)}` })
      );
    }

    if (pages.length === 1) {
      return interaction.editReply({ embeds: [pages[0]] });
    }

    return paginate(interaction, pages, { ephemeral: true });
  }

  // ── Use ───────────────────────────────────────────────────────────────────────
  if (sub === 'use') {
    const name = interaction.options.getString('name', true).toLowerCase();

    const tag = await prisma.customTag.findUnique({
      where: { guildId_name: { guildId, name } },
    });

    if (!tag) {
      return interaction.reply({
        embeds: [errorEmbed('Not Found', `No tag named \`${name}\` exists in this server.`)],
        ephemeral: true,
      });
    }

    // Increment uses
    prisma.customTag.update({ where: { id: tag.id }, data: { uses: { increment: 1 } } }).catch(() => null);

    const payload = buildTagPayload(tag, member, guild);
    return interaction.reply(payload);
  }

  // ── Info ──────────────────────────────────────────────────────────────────────
  if (sub === 'info') {
    await interaction.deferReply({ ephemeral: true });

    const name = interaction.options.getString('name', true).toLowerCase();
    const tag  = await prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });

    if (!tag) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', `No tag named \`${name}\`.`)] });
    }

    const embed = new EmbedBuilder()
      .setColor(COLORS.INFO)
      .setTitle(`Tag: ${tag.name}`)
      .addFields(
        { name: 'Created by', value: `<@${tag.createdBy}>`,                                                 inline: true },
        { name: 'Uses',       value: tag.uses.toLocaleString(),                                             inline: true },
        { name: 'Type',       value: tag.useEmbed ? '🖼️ Embed' : '📝 Text',                                inline: true },
        { name: 'Created',    value: `<t:${Math.floor(tag.createdAt.getTime() / 1000)}:R>`,                 inline: true },
        { name: 'Updated',    value: `<t:${Math.floor(tag.updatedAt.getTime() / 1000)}:R>`,                 inline: true },
        { name: 'Content',    value: tag.content.length > 1000 ? tag.content.slice(0, 997) + '…' : tag.content },
      )
      .setTimestamp();

    return interaction.editReply({ embeds: [embed] });
  }

  // ── Raw ───────────────────────────────────────────────────────────────────────
  if (sub === 'raw') {
    await interaction.deferReply({ ephemeral: true });

    const name = interaction.options.getString('name', true).toLowerCase();
    const tag  = await prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });

    if (!tag) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', `No tag named \`${name}\`.`)] });
    }

    // Escape backticks and send raw content in a code block
    const escaped = tag.content.replace(/`/g, '\\`').slice(0, 1900);
    return interaction.editReply({ content: `**Raw content for \`${tag.name}\`:**\n\`\`\`\n${escaped}\n\`\`\`` });
  }

  // ── Write operations — require Manage Guild or Manage Messages ────────────────
  if (!canWrite(interaction)) {
    return interaction.reply({
      embeds: [errorEmbed('No Permission', 'You need the **Manage Guild** or **Manage Messages** permission to create, edit, or delete tags.')],
      ephemeral: true,
    });
  }

  // ── Create ────────────────────────────────────────────────────────────────────
  if (sub === 'create') {
    await interaction.deferReply({ ephemeral: true });

    const rawName  = interaction.options.getString('name', true);
    const name     = rawName.toLowerCase().replace(/\s+/g, '-').replace(/[^a-z0-9-_]/g, '');
    const content  = interaction.options.getString('content', true);
    const useEmbed = interaction.options.getBoolean('embed') ?? false;

    if (!name) {
      return interaction.editReply({ embeds: [errorEmbed('Invalid Name', 'Tag names can only contain letters, numbers, hyphens, and underscores.')] });
    }

    const existing = await prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });
    if (existing) {
      return interaction.editReply({ embeds: [errorEmbed('Already Exists', `A tag named \`${name}\` already exists. Use \`/tag edit\` to update it.`)] });
    }

    await prisma.customTag.create({
      data: { guildId, name, content, useEmbed, createdBy: interaction.user.id },
    });

    return interaction.editReply({
      embeds: [successEmbed('Tag Created', `Tag \`${name}\` created.\nInvoke it with \`/tag use name:${name}\`.${useEmbed ? '\n🖼️ Will render as embed.' : ''}`)],
    });
  }

  // ── Edit ──────────────────────────────────────────────────────────────────────
  if (sub === 'edit') {
    await interaction.deferReply({ ephemeral: true });

    const name     = interaction.options.getString('name', true).toLowerCase();
    const content  = interaction.options.getString('content');
    const useEmbed = interaction.options.getBoolean('embed');

    if (!content && useEmbed === null) {
      return interaction.editReply({ embeds: [errorEmbed('Nothing to Change', 'Provide a new `content` or `embed` value to update.')] });
    }

    const tag = await prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });
    if (!tag) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', `No tag named \`${name}\`.`)] });
    }

    const updateData = {};
    if (content !== null)  updateData.content  = content;
    if (useEmbed !== null) updateData.useEmbed = useEmbed;

    await prisma.customTag.update({ where: { id: tag.id }, data: updateData });

    return interaction.editReply({
      embeds: [successEmbed('Tag Updated', `Tag \`${name}\` has been updated.`)],
    });
  }

  // ── Delete ────────────────────────────────────────────────────────────────────
  if (sub === 'delete') {
    await interaction.deferReply({ ephemeral: true });

    const name = interaction.options.getString('name', true).toLowerCase();
    const tag  = await prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });

    if (!tag) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', `No tag named \`${name}\`.`)] });
    }

    await prisma.customTag.delete({ where: { id: tag.id } });

    return interaction.editReply({
      embeds: [successEmbed('Tag Deleted', `Tag \`${name}\` (${tag.uses} uses) has been permanently deleted.`)],
    });
  }
}
