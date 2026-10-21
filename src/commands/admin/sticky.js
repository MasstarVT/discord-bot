// /sticky set | remove | list | pause | resume. Admin config command: replies are public (CLAUDE.md admin convention) and
// it keeps working while the Sticky Messages module is off (gated = false). The behaviour is src/plugins/sticky.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { getSavedEmbedTemplate, searchSavedEmbeds } from '../../services/embedService.js';
import {
  countStickies, dropChannel, embedPart, getSticky, listStickies, missingFor, removeSticky, setStickyEnabled,
} from '../../services/stickyService.js';
import { errorEmbed, guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { ackBudgetMs, TimeoutError, withTimeout } from '../../utils/async.js';
import { checkChannelUsable, respondAutocomplete, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import logger from '../../utils/logger.js';
import { applySet, buildStickyModal } from '../../plugins/sticky.js';

export const module = 'sticky';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const channelOption = (o, text) => o.setName('channel').setDescription(text).setRequired(true).addChannelTypes(...CHANNEL_TYPES);

export const data = new SlashCommandBuilder()
  .setName('sticky')
  .setDescription('Keep a message at the bottom of a channel.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('set').setDescription('Set or replace the sticky message in a channel.')
    .addChannelOption((o) => channelOption(o, 'The channel.'))
    .addStringOption((o) => o.setName('text').setDescription('The message ({server} works). Leave out to type it in a form.').setMaxLength(2000))
    .addStringOption((o) => o.setName('embed').setDescription('A saved embed (/embed) to post with it.').setMaxLength(100).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('remove').setDescription('Remove a channel\'s sticky message.')
    .addChannelOption((o) => channelOption(o, 'The channel.')))
  .addSubcommand((s) => s.setName('list').setDescription('Show every sticky message.'))
  .addSubcommand((s) => s.setName('pause').setDescription('Stop re-posting a sticky message (the current one is deleted).')
    .addChannelOption((o) => channelOption(o, 'The channel.')))
  .addSubcommand((s) => s.setName('resume').setDescription('Start re-posting a paused sticky message.')
    .addChannelOption((o) => channelOption(o, 'The channel.')));

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  UNAVAILABLE: () => errorEmbed('Temporarily Unavailable', 'That took too long. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  CANNOT_USE: (reason) => errorEmbed('Cannot Use That Channel', `${reason} Pick another channel, or fix my permissions there.`),
  LIMIT: () => errorEmbed('Limit Reached', `A server can have at most **${LIMITS.STICKY_MESSAGES}** sticky messages. Remove one with \`/sticky remove\` first.`),
  NONE: (channelId) => errorEmbed('No Sticky Message', `<#${channelId}> has no sticky message. Create one with \`/sticky set\`.`),
  NO_EMBED: (name) => errorEmbed('Embed Not Found', `There is no saved embed named \`${clampText(String(name).replace(/[`\\]/g, ''), 40)}\`. Create it with \`/embed create\`.`),
};

const OFF_NOTE = '\n\n⚠️ The **Sticky Messages** module is off. Turn it on with `/module enable sticky`.';

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

/** The channel option as the guild's cached channel, or null. */
function pickedChannel(interaction) {
  const option = interaction.options.getChannel('channel', true);
  return interaction.guild.channels.cache.get(option.id) ?? null;
}

// ── /sticky set ───────────────────────────────────────────────────────────────

/** No text and no embed: the form, prefilled with the current text. The form must be the first (un-deferred) answer. */
async function openForm(interaction, channel) {
  const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
  if (!usable.ok) return safeReply(interaction, { embeds: [MSG.CANNOT_USE(usable.reason)] });
  let existing;
  let count;
  try {
    [existing, count] = await withTimeout(
      Promise.all([getSticky(interaction.guildId, channel.id), countStickies(interaction.guildId)]),
      ackBudgetMs(interaction),
      'sticky set lookup',
    );
  } catch (err) {
    if (!(err instanceof TimeoutError)) logger.warn(`[sticky] ${interaction.guildId}: set lookup failed: ${err?.message}`);
    return safeReply(interaction, { embeds: [MSG.UNAVAILABLE()] });
  }
  if (!existing && count >= LIMITS.STICKY_MESSAGES) return safeReply(interaction, { embeds: [MSG.LIMIT()] });
  try {
    return await interaction.showModal(buildStickyModal(channel.id, existing?.content ?? null));
  } catch (err) {
    logger.debug(`[sticky] ${interaction.guildId}: could not show the form: ${err?.message}`);
    return undefined;
  }
}

async function runSet(interaction) {
  const channel = pickedChannel(interaction);
  const text = interaction.options.getString('text');
  const embedName = interaction.options.getString('embed');
  if (!channel) return safeReply(interaction, { embeds: [MSG.CANNOT_USE('Channel not found.')] });
  if (text === null && embedName === null) return openForm(interaction, channel);

  if (!(await safeDefer(interaction))) return undefined;
  // A slash option cannot hold a line break, so a typed \n becomes one.
  let content = text === null ? null : text.replace(/\\n/g, '\n');
  let part = null;
  if (embedName !== null) {
    const template = await getSavedEmbedTemplate(interaction.guildId, embedName);
    if (!template) return finish(interaction, MSG.NO_EMBED(embedName));
    part = embedPart(template);
    if (content === null) content = template.content ?? null;
  }
  return applySet(interaction, { guild: interaction.guild, channel, content, embed: part });
}

// ── /sticky remove | pause | resume ───────────────────────────────────────────

async function runRemove(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const channelId = interaction.options.getChannel('channel', true).id;
  let row;
  try {
    row = await removeSticky(interaction.client, interaction.guildId, channelId);
  } catch (err) {
    logger.error(`[sticky] ${interaction.guildId}: remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!row) return finish(interaction, MSG.NONE(channelId));
  logger.info(`[sticky] ${interaction.guildId}: ${channelId} removed by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Sticky Message Removed', `<#${channelId}> no longer has a sticky message.`));
}

async function runToggle(interaction, enabled) {
  if (!(await safeDefer(interaction))) return undefined;
  const channelId = interaction.options.getChannel('channel', true).id;
  let result;
  try {
    result = await setStickyEnabled(interaction.client, interaction.guildId, channelId, enabled);
  } catch (err) {
    logger.error(`[sticky] ${interaction.guildId}: ${enabled ? 'resume' : 'pause'} failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!result.found) return finish(interaction, MSG.NONE(channelId));
  logger.info(`[sticky] ${interaction.guildId}: ${channelId} ${enabled ? 'resumed' : 'paused'} by ${interaction.user.id}`);
  if (!enabled) {
    const note = result.changed ? 'The current sticky message was deleted.' : 'It was already paused.';
    return finish(interaction, successEmbed('Sticky Message Paused', `<#${channelId}> stays quiet until you run \`/sticky resume\`. ${note}`));
  }
  let note = 'It is posted again and will move back down once chat has been quiet.';
  if (result.status === 'off') note = OFF_NOTE.trim();
  else if (result.status === 'perms') note = "⚠️ I can't post there right now. Check my permissions in the channel.";
  else if (result.status === 'current') note = 'It is already the last message there.';
  else if (result.status !== 'sent') note = 'It will be posted after the next message.';
  return finish(interaction, successEmbed('Sticky Message Resumed', `<#${channelId}> is sticky again. ${note}`));
}

// ── /sticky list ──────────────────────────────────────────────────────────────

const preview = (content) => clampText(String(content ?? '').replace(/[`\\]/g, '').replace(/\s+/g, ' ').trim(), 50);

async function runList(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  let rows;
  try {
    rows = await listStickies(guildId);
    // A channel deleted while the bot was offline left its row behind: drop it (the cache is complete once the guild is).
    if (guild.channels.cache.size > 0) {
      const orphans = rows.filter((r) => !guild.channels.cache.has(r.channelId));
      for (const r of orphans) await dropChannel(guildId, r.channelId);
      if (orphans.length) rows = rows.filter((r) => !orphans.includes(r));
    }
  } catch (err) {
    logger.warn(`[sticky] ${guildId}: list failed: ${err?.message}`);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  const enabled = isModuleEnabled(settings, 'sticky');
  if (rows.length === 0) {
    return finish(interaction, guildEmbed(settings, '📌 Sticky Messages', `No sticky messages yet. Create one with \`/sticky set\`.${enabled ? '' : OFF_NOTE}`));
  }
  const lines = rows.map((r) => {
    const hasEmbed = Boolean(r.embed?.embed);
    const bits = [r.content ? `\`${preview(r.content) || '…'}\`` : null, hasEmbed ? '🖼️ embed' : null].filter(Boolean);
    const channel = guild.channels.cache.get(r.channelId);
    const missing = channel ? missingFor(channel, hasEmbed) : [];
    const problem = r.enabled && missing.length ? ` ⚠️ I need ${missing.join(', ')}` : '';
    return `${r.enabled ? '✅' : '⏸️'} <#${r.channelId}> ${bits.join(' + ') || '(empty)'}${problem}`;
  });
  const head = enabled ? '' : `${OFF_NOTE.trim()}\n\n`;
  const embed = guildEmbed(settings, '📌 Sticky Messages', `${head}${joinWithLimit(lines, { sep: '\n', max: 3500 })}`)
    .setFooter({ text: `${rows.length} of ${LIMITS.STICKY_MESSAGES} used` });
  return finish(interaction, embed);
}

// ── Autocomplete and dispatch ─────────────────────────────────────────────────

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name !== 'embed') return await respondAutocomplete(interaction, []);
    const rows = await searchSavedEmbeds(interaction.guildId, String(focused.value), 25);
    return await respondAutocomplete(interaction, rows.map((r) => ({ name: r.name, value: r.name })));
  } catch (err) {
    logger.debug(`[sticky] autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'set') return runSet(interaction);
  if (sub === 'remove') return runRemove(interaction);
  if (sub === 'list') return runList(interaction);
  if (sub === 'pause') return runToggle(interaction, false);
  if (sub === 'resume') return runToggle(interaction, true);
  return undefined;
}
