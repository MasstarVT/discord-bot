// /suggestion: member suggestions with voting (beyond MEE6). Members run `submit`; moderators run approve | deny | consider |
// implement | delete (subcommandPermissions); admins run `setup`. The command is not gated, so admins can set the channel up
// before turning the module on; `submit` checks the module itself and refuses with the router's wording while it is off.
// The rules (post, votes, review) live in services/suggestionService.js.
import { SlashCommandBuilder, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import {
  getSettingsSafe, isModuleEnabled, peekSettings, updateSettings,
} from '../../services/settingsService.js';
import {
  STATUS_INFO, SUGGESTION, buildSuggestionModal, cooldownEmbed, cooldownLeftMs, deleteSuggestion, jumpUrl, reviewSuggestion,
  submitFromInteraction, submitProblem,
} from '../../services/suggestionService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  checkChannelUsable, isInteractionGone, missingBotPermissions, permissionNames, safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { newChannelOption, resolveNewOptions } from '../../utils/newTargets.js';

export const module = 'suggestions';
export const gated = false;                                  // `setup` and the review commands work while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER;     // submit
export const subcommandPermissions = Object.freeze({
  approve: PERMISSION_LEVELS.MODERATOR,
  deny: PERMISSION_LEVELS.MODERATOR,
  consider: PERMISSION_LEVELS.MODERATOR,
  implement: PERMISSION_LEVELS.MODERATOR,
  delete: PERMISSION_LEVELS.MODERATOR,
  setup: PERMISSION_LEVELS.ADMIN,
});
// No setDefaultMemberPermissions and no allowInDMs: members must see `submit`; guild only.

const REVIEWS = Object.freeze({
  approve: { status: 'APPROVED', blurb: 'Approve a suggestion (closes voting).' },
  deny: { status: 'DENIED', blurb: 'Deny a suggestion (closes voting).' },
  consider: { status: 'CONSIDERED', blurb: 'Mark a suggestion as under consideration (voting stays open).' },
  implement: { status: 'IMPLEMENTED', blurb: 'Mark a suggestion as implemented (closes voting).' },
});

const NUMBER = (o) => o.setName('number').setDescription('The suggestion number').setRequired(true).setMinValue(1).setMaxValue(2_000_000_000);

export const data = (() => {
  const builder = new SlashCommandBuilder()
    .setName('suggestion')
    .setDescription('Post a suggestion for the server to vote on, or review suggestions.')
    .addSubcommand((s) => s.setName('submit').setDescription('Post a suggestion. Leave the text empty to write it in a form.')
      .addStringOption((o) => o.setName('text').setDescription('Your suggestion').setMaxLength(LIMITS.SUGGESTION_LENGTH))
      .addBooleanOption((o) => o.setName('anonymous').setDescription('Hide your name on the post (if the server allows it)')));
  for (const [name, { blurb }] of Object.entries(REVIEWS)) {
    builder.addSubcommand((s) => s.setName(name).setDescription(blurb)
      .addIntegerOption(NUMBER)
      .addStringOption((o) => o.setName('note').setDescription('A note for the author and the post').setMaxLength(SUGGESTION.NOTE_MAX)));
  }
  return builder
    .addSubcommand((s) => s.setName('delete').setDescription('Delete a suggestion and its post.').addIntegerOption(NUMBER))
    .addSubcommand((s) => s.setName('setup').setDescription('Set the suggestion channel and options (no options: show them).')
      .addChannelOption((o) => o.setName('channel').setDescription('Where suggestions are posted')
        .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addStringOption((o) => newChannelOption(o))
      .addBooleanOption((o) => o.setName('threads').setDescription('Open a discussion thread on each suggestion'))
      .addBooleanOption((o) => o.setName('anonymous').setDescription('Let members post anonymously'))
      .addBooleanOption((o) => o.setName('dm_on_update').setDescription('DM the author when staff review their suggestion'))
      .addIntegerOption((o) => o.setName('cooldown').setDescription('Seconds between suggestions per member (0 = none)')
        .setMinValue(0).setMaxValue(86_400)));
})();

const NO_MENTIONS = Object.freeze({ parse: [] });
const SETTINGS_UNAVAILABLE = () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const notFound = (n) => errorEmbed('Suggestion Not Found', `This server has no suggestion **#${n}**.`);

// ── submit ────────────────────────────────────────────────────────────────────

async function submit(interaction, client) {
  const text = interaction.options.getString('text');
  const anonymous = interaction.options.getBoolean('anonymous') ?? false;
  const { guildId } = interaction;

  if (text === null) {
    // The form must be the first response: only bounded lookups before it. Without settings in time the form opens anyway and
    // the submit re-checks everything.
    const settings = peekSettings(guildId)
      ?? (ackBudgetMs(interaction) > 0
        ? await withTimeout(getSettingsSafe(guildId), ackBudgetMs(interaction), 'suggestion settings').catch(() => null)
        : null);
    let problem = settings ? submitProblem(settings, { anonymous }) : null;
    if (!problem && settings && ackBudgetMs(interaction) > 0) {
      const left = await withTimeout(cooldownLeftMs(guildId, interaction.user.id, settings), ackBudgetMs(interaction), 'suggestion cooldown')
        .catch(() => 0);
      if (left > 0) problem = cooldownEmbed(left);
    }
    if (problem) {
      await safeReply(interaction, { embeds: [problem], ephemeral: true });
      return;
    }
    try {
      await interaction.showModal(buildSuggestionModal(anonymous));
    } catch (err) {
      if (!isInteractionGone(err)) logger.error('suggestion submit: showModal failed', err);
    }
    return;
  }

  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const settings = await getSettingsSafe(guildId);
  await submitFromInteraction(interaction, client, { content: text, anonymous, settings });
}

// ── approve | deny | consider | implement ─────────────────────────────────────

async function review(interaction, client, sub) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guild, guildId } = interaction;
  const number = interaction.options.getInteger('number', true);
  const settings = await getSettingsSafe(guildId);
  if (!settings) return reply(SETTINGS_UNAVAILABLE());

  let out;
  try {
    out = await reviewSuggestion({
      client, guild, number, status: REVIEWS[sub].status, note: interaction.options.getString('note'), reviewer: interaction.user, settings,
    });
  } catch (err) {
    logger.error(`[suggestions] ${guildId}: review of #${number} failed`, err);
    return reply(errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'));
  }
  if (!out.ok) return reply(notFound(number));

  const s = out.suggestion;
  const info = STATUS_INFO[s.status];
  const url = jumpUrl(s);
  const lines = [`Author: <@${s.authorId}>${s.anonymous ? ' (anonymous on the post)' : ''}`];
  if (s.reviewNote) lines.push(`Note: ${s.reviewNote}`);
  lines.push(out.edited
    ? `The post was updated${url ? ` ([jump](${url}))` : ''}.`
    : '⚠️ I could not update the post: it was deleted or I cannot edit it. The status is saved.');
  if (out.dm === 'sent') lines.push('The author was sent a DM.');
  else if (out.dm === 'failed') lines.push('⚠️ The author could not be reached by DM (their DMs are closed).');
  if (!isModuleEnabled(settings, 'suggestions')) lines.push('⚠️ The Suggestions module is off. Turn it on with `/module enable suggestions`.');
  return reply(successEmbed(`Suggestion #${number}: ${info.label}`, lines.join('\n')));
}

// ── delete ────────────────────────────────────────────────────────────────────

async function remove(interaction, client) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const number = interaction.options.getInteger('number', true);
  let out;
  try {
    out = await deleteSuggestion({ client, guild: interaction.guild, number });
  } catch (err) {
    logger.error(`[suggestions] ${interaction.guildId}: delete of #${number} failed`, err);
    return reply(errorEmbed('Could Not Delete', 'The suggestion could not be deleted. Please try again in a moment.'));
  }
  if (!out.ok) {
    return reply(out.code === 'NOT_FOUND'
      ? notFound(number)
      : errorEmbed('Could Not Delete', `I couldn't delete the post: ${out.data?.reason ?? 'unknown error.'} Nothing was changed.`));
  }
  logger.info(`[suggestions] ${interaction.guildId}: #${number} deleted by ${interaction.user.id}`);
  return reply(successEmbed('Suggestion Deleted', `Suggestion **#${number}** and its votes were deleted.`));
}

// ── setup ─────────────────────────────────────────────────────────────────────

const yesNo = (v) => (v ? 'Yes' : 'No');

function configEmbed(settings, title) {
  const channelId = settings.suggestionChannelId;
  const cooldown = settings.suggestionCooldownSeconds ?? 0;
  return guildEmbed(settings, title, [
    `Module: ${isModuleEnabled(settings, 'suggestions') ? '✅ On' : '❌ Off, turn it on with `/module enable suggestions`'}`,
    `Channel: ${channelId ? `<#${channelId}>` : 'Not set'}`,
    `Discussion threads: ${yesNo(settings.suggestionThreads)}`,
    `Anonymous suggestions: ${yesNo(settings.suggestionAllowAnonymous)}`,
    `DM the author on review: ${yesNo(settings.suggestionDmOnUpdate)}`,
    `Cooldown: ${cooldown > 0 ? `${cooldown} seconds` : 'none'}`,
  ].join('\n'));
}

async function setup(interaction) {
  if (!(await safeDefer(interaction))) return;
  if (!(await resolveNewOptions(interaction, data, { required: [] }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guild, guildId, options } = interaction;

  const settings = await getSettingsSafe(guildId);
  if (!settings) return reply(SETTINGS_UNAVAILABLE());

  const update = {};
  const picked = options.getChannel('channel');
  const threads = options.getBoolean('threads');
  const anonymous = options.getBoolean('anonymous');
  const dm = options.getBoolean('dm_on_update');
  const cooldown = options.getInteger('cooldown');
  if (threads !== null) update.suggestionThreads = threads;
  if (anonymous !== null) update.suggestionAllowAnonymous = anonymous;
  if (dm !== null) update.suggestionDmOnUpdate = dm;
  if (cooldown !== null) update.suggestionCooldownSeconds = cooldown;

  let channel = null;
  if (picked) {
    channel = guild.channels.cache.get(picked.id) ?? null;
    const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
    if (!usable.ok) return reply(errorEmbed('Cannot Use That Channel', `${usable.reason} Pick another channel, or fix my permissions there.`));
    update.suggestionChannelId = channel.id;
  }
  if (!Object.keys(update).length) return reply(configEmbed(settings, '💡 Suggestions'));

  let saved;
  try {
    saved = await updateSettings(guildId, update);
  } catch (err) {
    logger.error(`[suggestions] ${guildId}: setup failed`, err);
    return reply(errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'));
  }
  logger.info(`[suggestions] ${guildId}: setup changed ${Object.keys(update).join(', ')} by ${interaction.user.id}`);

  const embed = configEmbed(saved, '✅ Suggestions Updated');
  // The thread option needs a permission the post itself does not.
  const target = channel ?? (saved.suggestionChannelId ? guild.channels.cache.get(saved.suggestionChannelId) : null);
  if (saved.suggestionThreads && target) {
    const missing = missingBotPermissions(target, ['CreatePublicThreads']);
    if (missing.length) embed.addFields({ name: 'Threads', value: `⚠️ I need **${permissionNames(missing).join(', ')}** in <#${target.id}> to open threads.` });
  }
  if (!saved.suggestionChannelId) embed.addFields({ name: 'Channel', value: '⚠️ Pick a channel with the `channel` option, or nobody can post.' });
  return reply(embed);
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'submit') return submit(interaction, client);
  if (sub === 'delete') return remove(interaction, client);
  if (sub === 'setup') return setup(interaction);
  if (Object.hasOwn(REVIEWS, sub)) return review(interaction, client, sub);
  return undefined;
}
