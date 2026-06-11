import {
  SlashCommandBuilder,
  PermissionFlagsBits,
  EmbedBuilder,
} from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { invalidateTriggerCache } from '../../services/tagService.js';
import { paginate } from '../../utils/paginator.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const MATCH_LABELS = {
  CONTAINS:    'Contains (default)',
  EXACT:       'Exact match',
  STARTS_WITH: 'Starts with',
  ENDS_WITH:   'Ends with',
  REGEX:       'Regex',
};

export const data = new SlashCommandBuilder()
  .setName('trigger')
  .setDescription('Manage auto-response triggers and the custom commands module.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)

  // ── Module toggles ────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('enable').setDescription('Enable the custom commands & triggers module.')
  )
  .addSubcommand((s) =>
    s.setName('disable').setDescription('Disable the custom commands & triggers module.')
  )

  // ── Add trigger ───────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('add')
      .setDescription('Add a new auto-response trigger.')
      .addStringOption((o) =>
        o.setName('pattern').setDescription('Keyword or pattern to watch for').setRequired(true).setMaxLength(500)
      )
      .addStringOption((o) =>
        o.setName('response').setDescription('Message the bot sends when triggered (supports placeholders)').setRequired(true).setMaxLength(2000)
      )
      .addStringOption((o) =>
        o.setName('match_type')
          .setDescription('How to match the pattern (default: Contains)')
          .addChoices(
            { name: 'Contains — pattern appears anywhere in message', value: 'CONTAINS'    },
            { name: 'Exact    — message equals pattern exactly',      value: 'EXACT'       },
            { name: 'Starts with',                                     value: 'STARTS_WITH' },
            { name: 'Ends with',                                       value: 'ENDS_WITH'   },
            { name: 'Regex    — advanced pattern (use carefully)',     value: 'REGEX'       },
          )
      )
      .addIntegerOption((o) =>
        o.setName('cooldown')
          .setDescription('Seconds before the same user can re-trigger (0 = no cooldown)')
          .setMinValue(0).setMaxValue(86400)
      )
      .addBooleanOption((o) =>
        o.setName('delete_message').setDescription('Delete the triggering message? (default: false)')
      )
      .addBooleanOption((o) =>
        o.setName('case_sensitive').setDescription('Case-sensitive matching? (default: false)')
      )
      .addChannelOption((o) =>
        o.setName('channel').setDescription('Only fire in this channel (omit for all channels)')
      )
  )

  // ── Remove trigger ────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('remove')
      .setDescription('Remove a trigger permanently.')
      .addStringOption((o) =>
        o.setName('trigger').setDescription('Trigger to remove').setRequired(true).setAutocomplete(true)
      )
  )

  // ── Toggle trigger ────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('toggle')
      .setDescription('Enable or disable a specific trigger without deleting it.')
      .addStringOption((o) =>
        o.setName('trigger').setDescription('Trigger to toggle').setRequired(true).setAutocomplete(true)
      )
  )

  // ── Info ──────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('info')
      .setDescription('Show full details for a trigger.')
      .addStringOption((o) =>
        o.setName('trigger').setDescription('Trigger to inspect').setRequired(true).setAutocomplete(true)
      )
  )

  // ── List ──────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('list').setDescription('List all triggers in this server.')
  );

// ── Autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  const focused  = interaction.options.getFocused().toLowerCase();
  const guildId  = interaction.guildId;

  const triggers = await prisma.autoTrigger.findMany({
    where:   { guildId },
    orderBy: { createdAt: 'desc' },
    take:    25,
    select:  { id: true, trigger: true, matchType: true, enabled: true },
  });

  const filtered = focused
    ? triggers.filter((t) => t.trigger.toLowerCase().includes(focused))
    : triggers;

  await interaction.respond(
    filtered.slice(0, 25).map((t) => ({
      name:  `${t.enabled ? '✅' : '❌'} [${t.matchType}] ${t.trigger.slice(0, 80)}`,
      value: t.id,
    }))
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async function bust(guildId) {
  await cache.del(`guild:${guildId}:settings`);
}

async function upsertSettings(guildId, data) {
  await prisma.guildSettings.upsert({
    where:  { guildId },
    update: data,
    create: { guildId, ...data },
  });
  await bust(guildId);
}

// ── Execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  await interaction.deferReply();

  const sub     = interaction.options.getSubcommand();
  const guildId = interaction.guildId;

  // ── Enable / Disable module ────────────────────────────────────────────────
  if (sub === 'enable') {
    await upsertSettings(guildId, { customCommandsEnabled: true });
    return interaction.editReply({
      embeds: [successEmbed('Module Enabled', 'Custom commands & triggers are now active.\nTags respond to `/tag use`, triggers fire on matching messages.')],
    });
  }

  if (sub === 'disable') {
    await upsertSettings(guildId, { customCommandsEnabled: false });
    return interaction.editReply({
      embeds: [successEmbed('Module Disabled', 'Custom commands & triggers have been disabled. Existing data is preserved.')],
    });
  }

  // ── Add trigger ────────────────────────────────────────────────────────────
  if (sub === 'add') {
    const pattern       = interaction.options.getString('pattern', true);
    const response      = interaction.options.getString('response', true);
    const matchType     = interaction.options.getString('match_type') ?? 'CONTAINS';
    const cooldown      = interaction.options.getInteger('cooldown') ?? 0;
    const deleteMessage = interaction.options.getBoolean('delete_message') ?? false;
    const caseSensitive = interaction.options.getBoolean('case_sensitive') ?? false;
    const channel       = interaction.options.getChannel('channel');

    // Validate regex if provided
    if (matchType === 'REGEX') {
      try { new RegExp(pattern); } catch (e) {
        return interaction.editReply({
          embeds: [errorEmbed('Invalid Regex', `The pattern \`${pattern}\` is not a valid regular expression.\n> ${e.message}`)],
        });
      }
    }

    const trigger = await prisma.autoTrigger.create({
      data: {
        guildId,
        trigger:       pattern,
        response,
        matchType,
        cooldown,
        deleteMessage,
        caseSensitive,
        channelIds:   channel ? [channel.id] : [],
        createdBy:    interaction.user.id,
      },
    });

    await invalidateTriggerCache(guildId);

    const details = [
      `**Pattern:** \`${pattern}\``,
      `**Match:** ${MATCH_LABELS[matchType]}${caseSensitive ? ' (case-sensitive)' : ''}`,
      `**Cooldown:** ${cooldown > 0 ? `${cooldown}s per user` : 'None'}`,
      `**Delete message:** ${deleteMessage ? 'Yes' : 'No'}`,
      channel ? `**Channel:** <#${channel.id}>` : '**Channel:** All channels',
    ].join('\n');

    return interaction.editReply({
      embeds: [successEmbed('Trigger Added', details + `\n\nID: \`${trigger.id}\``)],
    });
  }

  // ── Remove trigger ─────────────────────────────────────────────────────────
  if (sub === 'remove') {
    const triggerId = interaction.options.getString('trigger', true);

    const trigger = await prisma.autoTrigger.findUnique({ where: { id: triggerId } });
    if (!trigger || trigger.guildId !== guildId) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', 'Trigger not found.')] });
    }

    await prisma.autoTrigger.delete({ where: { id: triggerId } });
    await invalidateTriggerCache(guildId);

    return interaction.editReply({
      embeds: [successEmbed('Trigger Removed', `Trigger for \`${trigger.trigger}\` has been deleted (${trigger.uses} fires total).`)],
    });
  }

  // ── Toggle trigger ─────────────────────────────────────────────────────────
  if (sub === 'toggle') {
    const triggerId = interaction.options.getString('trigger', true);

    const trigger = await prisma.autoTrigger.findUnique({ where: { id: triggerId } });
    if (!trigger || trigger.guildId !== guildId) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', 'Trigger not found.')] });
    }

    const updated = await prisma.autoTrigger.update({
      where: { id: triggerId },
      data:  { enabled: !trigger.enabled },
    });

    await invalidateTriggerCache(guildId);

    return interaction.editReply({
      embeds: [successEmbed(
        `Trigger ${updated.enabled ? 'Enabled' : 'Disabled'}`,
        `\`${trigger.trigger}\` is now **${updated.enabled ? 'active' : 'paused'}**.`
      )],
    });
  }

  // ── Info ───────────────────────────────────────────────────────────────────
  if (sub === 'info') {
    const triggerId = interaction.options.getString('trigger', true);

    const trigger = await prisma.autoTrigger.findUnique({ where: { id: triggerId } });
    if (!trigger || trigger.guildId !== guildId) {
      return interaction.editReply({ embeds: [errorEmbed('Not Found', 'Trigger not found.')] });
    }

    const embed = new EmbedBuilder()
      .setColor(trigger.enabled ? COLORS.SUCCESS : COLORS.NEUTRAL)
      .setTitle(`Trigger: ${trigger.trigger.slice(0, 50)}`)
      .addFields(
        { name: 'Match Type',     value: MATCH_LABELS[trigger.matchType],                                                    inline: true },
        { name: 'Status',         value: trigger.enabled ? '✅ Active' : '❌ Paused',                                        inline: true },
        { name: 'Uses',           value: trigger.uses.toLocaleString(),                                                      inline: true },
        { name: 'Cooldown',       value: trigger.cooldown > 0 ? `${trigger.cooldown}s` : 'None',                            inline: true },
        { name: 'Delete message', value: trigger.deleteMessage ? 'Yes' : 'No',                                              inline: true },
        { name: 'Case sensitive', value: trigger.caseSensitive ? 'Yes' : 'No',                                             inline: true },
        { name: 'Channels',       value: trigger.channelIds.length ? trigger.channelIds.map((id) => `<#${id}>`).join(', ') : 'All channels', inline: false },
        { name: 'Pattern',        value: `\`\`\`\n${trigger.trigger.slice(0, 900)}\n\`\`\``,                               inline: false },
        { name: 'Response',       value: trigger.response.length > 900 ? trigger.response.slice(0, 897) + '…' : trigger.response, inline: false },
      )
      .setFooter({ text: `ID: ${trigger.id} · Created by <@${trigger.createdBy}>` })
      .setTimestamp(trigger.createdAt);

    return interaction.editReply({ embeds: [embed] });
  }

  // ── List ───────────────────────────────────────────────────────────────────
  if (sub === 'list') {
    const triggers = await prisma.autoTrigger.findMany({
      where:   { guildId },
      orderBy: { createdAt: 'asc' },
    });

    if (triggers.length === 0) {
      return interaction.editReply({
        embeds: [infoEmbed('No Triggers', 'No triggers configured.\nAdd one with `/trigger add`.')],
      });
    }

    const PAGE = 10;
    const pages = [];
    for (let i = 0; i < triggers.length; i += PAGE) {
      const chunk = triggers.slice(i, i + PAGE);
      pages.push(
        new EmbedBuilder()
          .setColor(COLORS.INFO)
          .setTitle(`⚡ Auto-Triggers (${triggers.length} total)`)
          .setDescription(
            chunk.map((t) => [
              `${t.enabled ? '✅' : '❌'} **[${t.matchType}]** \`${t.trigger.slice(0, 40)}${t.trigger.length > 40 ? '…' : ''}\``,
              `↳ ${t.response.slice(0, 60)}${t.response.length > 60 ? '…' : ''} · ${t.uses} fires`,
            ].join('\n')).join('\n\n')
          )
          .setFooter({ text: `Page ${Math.floor(i / PAGE) + 1} of ${Math.ceil(triggers.length / PAGE)}` })
      );
    }

    if (pages.length === 1) {
      return interaction.editReply({ embeds: [pages[0]] });
    }

    return paginate(interaction, pages);
  }
}
