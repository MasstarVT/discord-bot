// /confession: anonymous confessions (beyond MEE6). Members run `submit` (text option, or an empty one for a form); moderators run
// delete | block | unblock (subcommandPermissions); admins run `reveal`, `panel` and `setup`. The command is not gated, so admins can
// set the channels up before turning the module on; `submit` checks the module itself and refuses with the router's wording while
// it is off. EVERY reply is ephemeral: nothing public may hint who used the command. The rules live in services/confessionService.js.
import { SlashCommandBuilder, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { getSettingsSafe, isModuleEnabled, updateSettings } from '../../services/settingsService.js';
import {
  deleteConfession, describeAuthor, errTag, buildPanelPayload, openConfessionForm, revealConfession, setBlocked,
  submitFromInteraction,
} from '../../services/confessionService.js';
import { errorEmbed, guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { checkChannelUsable, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { everyoneCanView } from '../../utils/privacyGuards.js';

export const module = 'confessions';
export const gated = false;                                  // `setup` and the staff commands work while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER;     // submit
export const subcommandPermissions = Object.freeze({
  panel: PERMISSION_LEVELS.ADMIN,
  delete: PERMISSION_LEVELS.MODERATOR,
  block: PERMISSION_LEVELS.MODERATOR,
  unblock: PERMISSION_LEVELS.MODERATOR,
  reveal: PERMISSION_LEVELS.ADMIN,
  setup: PERMISSION_LEVELS.ADMIN,
});
// No setDefaultMemberPermissions and no allowInDMs: members must see `submit`; guild only.

const NUMBER = (o) => o.setName('number').setDescription('The confession number').setRequired(true).setMinValue(1).setMaxValue(2_000_000_000);
const TEXT_CHANNELS = [ChannelType.GuildText, ChannelType.GuildAnnouncement];

export const data = new SlashCommandBuilder()
  .setName('confession')
  .setDescription('Post an anonymous confession, or manage confessions.')
  .addSubcommand((s) => s.setName('submit').setDescription('Post a confession. Leave the text empty to write it in a form.')
    .addStringOption((o) => o.setName('text').setDescription('Your confession').setMaxLength(LIMITS.CONFESSION_LENGTH))
    .addAttachmentOption((o) => o.setName('image').setDescription('An image to show with it (if the server allows it)')))
  .addSubcommand((s) => s.setName('panel').setDescription('Post a message with a Confess button.')
    .addChannelOption((o) => o.setName('channel').setDescription('Where to post it (default: here)').addChannelTypes(...TEXT_CHANNELS)))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a confession and its post.').addIntegerOption(NUMBER))
  .addSubcommand((s) => s.setName('block').setDescription('Stop the author of a confession from confessing. You will not see who.')
    .addIntegerOption(NUMBER))
  .addSubcommand((s) => s.setName('unblock').setDescription('Let the author of a confession confess again.').addIntegerOption(NUMBER))
  .addSubcommand((s) => s.setName('reveal').setDescription('Show who wrote a confession. Written to the moderation log.')
    .addIntegerOption(NUMBER))
  .addSubcommand((s) => s.setName('setup').setDescription('Set the confession and review channels (no options: show them).')
    .addChannelOption((o) => o.setName('channel').setDescription('Where confessions are posted').addChannelTypes(...TEXT_CHANNELS))
    .addChannelOption((o) => o.setName('review_channel').setDescription('Where moderators approve them first (keep it private)')
      .addChannelTypes(...TEXT_CHANNELS))
    .addBooleanOption((o) => o.setName('no_review').setDescription('Stop reviewing: post confessions at once')));

const NO_MENTIONS = Object.freeze({ parse: [] });
const SETTINGS_UNAVAILABLE = () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const notFound = (n) => errorEmbed('Confession Not Found', `This server has no confession **#${n}**.`);
const TRY_AGAIN = () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.');

// ── submit ────────────────────────────────────────────────────────────────────

async function submit(interaction, client) {
  const text = interaction.options.getString('text');
  const attachment = interaction.options.getAttachment('image');

  if (text === null) {
    // The form has no image field: an image needs the text option next to it.
    if (attachment) {
      await safeReply(interaction, {
        embeds: [errorEmbed('Add The Text', 'Type your confession in the `text` option when you attach an image. The form cannot take images.')],
        ephemeral: true,
      });
      return;
    }
    await openConfessionForm(interaction);
    return;
  }
  // Ephemeral: the invocation must not show in the channel.
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  await submitFromInteraction(interaction, client, { content: text, attachment, settings });
}

// ── delete | block | unblock | reveal ─────────────────────────────────────────

async function remove(interaction, client) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const number = interaction.options.getInteger('number', true);
  let out;
  try {
    const settings = await getSettingsSafe(interaction.guildId);
    out = await deleteConfession({ client, guild: interaction.guild, number, settings });
  } catch (err) {
    logger.error(`[confessions] ${interaction.guildId}: delete of #${number} failed (${errTag(err)})`);
    return reply(errorEmbed('Could Not Delete', 'The confession could not be deleted. Please try again in a moment.'));
  }
  if (!out.ok) {
    if (out.code === 'NOT_FOUND') return reply(notFound(number));
    if (out.code === 'ALREADY') return reply(errorEmbed('Already Deleted', `Confession **#${number}** was already deleted.`));
    if (out.code === 'CHANGED') return reply(errorEmbed('Try Again', `Confession **#${number}** was just changed by someone else. Run the command again.`));
    return reply(errorEmbed('Could Not Delete', `I couldn't delete the post: ${out.data?.reason ?? 'unknown error.'} Nothing was changed.`));
  }
  logger.info(`[confessions] ${interaction.guildId}: #${number} deleted by ${interaction.user.id}`);
  return reply(successEmbed('Confession Deleted', `Confession **#${number}** was deleted.`));
}

async function block(interaction, blocked) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const number = interaction.options.getInteger('number', true);
  let out;
  try {
    out = await setBlocked({ guildId: interaction.guildId, number, blocked });
  } catch (err) {
    logger.error(`[confessions] ${interaction.guildId}: ${blocked ? 'block' : 'unblock'} of #${number} failed (${errTag(err)})`);
    return reply(TRY_AGAIN());
  }
  if (!out.ok) {
    return reply(out.code === 'NOT_FOUND'
      ? notFound(number)
      : errorEmbed('Blocklist Full', `Up to **${LIMITS.CONFESSION_BLOCKED_USERS}** members can be blocked. Unblock someone first.`));
  }
  logger.info(`[confessions] ${interaction.guildId}: #${number} ${blocked ? 'blocked' : 'unblocked'} by ${interaction.user.id}`);
  // The same words whatever the state was before, and never a name: this must not tell two confessions apart by author.
  return reply(successEmbed(
    blocked ? 'Author Blocked' : 'Author Unblocked',
    blocked
      ? `The author of confession **#${number}** can no longer post confessions. You do not see who they are.`
      : `The author of confession **#${number}** can post confessions again.`,
  ));
}

async function reveal(interaction, client) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const number = interaction.options.getInteger('number', true);
  let out;
  try {
    out = await revealConfession({ guild: interaction.guild, number, admin: interaction.user });
  } catch (err) {
    logger.error(`[confessions] ${interaction.guildId}: reveal of #${number} failed (${errTag(err)})`);
    return reply(errorEmbed('Could Not Reveal', 'Something went wrong, so nothing was revealed. Please try again in a moment.'));
  }
  if (!out.ok) {
    if (out.code === 'NOT_FOUND') return reply(notFound(number));
    const why = out.code === 'NO_LOG'
      ? `${out.data?.reason ?? 'The moderation log is unavailable.'} Set one with \`/modconfig logchannel\` (and let me post there) first.`
      : `I couldn't write the log entry: ${out.data?.reason ?? 'unknown error.'}`;
    return reply(errorEmbed('Not Revealed', `Every reveal is written to the moderation log first, so nothing was revealed. ${why}`));
  }
  const author = await describeAuthor(client, interaction.guild, out.confession.authorId);
  const lines = [
    `Confession **#${number}** was written by <@${author.id}>${author.name ? ` (**${author.name}**)` : ''}.`,
    `User ID: \`${author.id}\``,
    author.inServer ? '' : 'They are no longer in this server.',
    'This lookup was written to the moderation log.',
  ].filter(Boolean);
  return reply(successEmbed('Author Revealed', lines.join('\n')));
}

// ── panel ─────────────────────────────────────────────────────────────────────

async function panel(interaction) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const picked = interaction.options.getChannel('channel');
  const channel = picked ? (interaction.guild.channels.cache.get(picked.id) ?? null) : interaction.channel;
  const usable = checkChannelUsable(channel);
  if (!usable.ok) return reply(errorEmbed('Cannot Use That Channel', `${usable.reason} Pick another channel, or fix my permissions there.`));
  try {
    await channel.send(buildPanelPayload());
  } catch (err) {
    logger.warn(`[confessions] ${interaction.guildId}: could not post the panel: ${err?.message}`);
    return reply(errorEmbed('Could Not Post', `I couldn't post the panel in <#${channel.id}>: ${err?.message ?? 'unknown error'}.`));
  }
  const settings = await getSettingsSafe(interaction.guildId);
  const notes = [`The Confess button is in <#${channel.id}>.`];
  if (!settings?.confessionChannelId) notes.push('⚠️ No confession channel is set yet. Pick one with `/confession setup`, or the button will refuse.');
  if (settings && !isModuleEnabled(settings, 'confessions')) notes.push('⚠️ The Confessions module is off. Turn it on with `/module enable confessions`.');
  return reply(successEmbed('Panel Posted', notes.join('\n')));
}

// ── setup ─────────────────────────────────────────────────────────────────────

function configEmbed(settings, title) {
  const channelId = settings.confessionChannelId;
  const reviewId = settings.confessionReviewChannelId;
  return guildEmbed(settings, title, [
    `Module: ${isModuleEnabled(settings, 'confessions') ? '✅ On' : '❌ Off, turn it on with `/module enable confessions`'}`,
    `Channel: ${channelId ? `<#${channelId}>` : 'Not set'}`,
    `Review channel: ${reviewId ? `<#${reviewId}>` : 'None, confessions are posted at once'}`,
    `Images: ${settings.confessionAllowImages ? 'Allowed' : 'Not allowed'}`,
    `Links: ${settings.confessionBlockLinks ? 'Refused' : 'Allowed'}`,
    `Cooldown: ${settings.confessionCooldownSeconds > 0 ? `${settings.confessionCooldownSeconds} seconds` : 'none'}`,
    'Blocked members: not listed (so nobody can tell who they are or whether two confessions share an author)',
  ].join('\n'));
}

async function setup(interaction) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guild, guildId, options } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return reply(SETTINGS_UNAVAILABLE());

  const pickedChannel = options.getChannel('channel');
  const pickedReview = options.getChannel('review_channel');
  const noReview = options.getBoolean('no_review');
  if (pickedReview && noReview) {
    return reply(errorEmbed('Pick One', 'Choose a `review_channel`, or set `no_review`, not both.'));
  }

  const update = {};
  const resolved = {};
  for (const [key, picked, field] of [['channel', pickedChannel, 'confessionChannelId'], ['review', pickedReview, 'confessionReviewChannelId']]) {
    if (!picked) continue;
    const ch = guild.channels.cache.get(picked.id) ?? null;
    const usable = checkChannelUsable(ch, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles'] });
    if (!usable.ok) return reply(errorEmbed('Cannot Use That Channel', `${usable.reason} Pick another channel, or fix my permissions there.`));
    update[field] = ch.id;
    resolved[key] = ch;
  }
  if (noReview) update.confessionReviewChannelId = null;
  if (!Object.keys(update).length) return reply(configEmbed(settings, '🤫 Confessions'));

  const channelId = update.confessionChannelId ?? settings.confessionChannelId;
  const reviewId = 'confessionReviewChannelId' in update ? update.confessionReviewChannelId : settings.confessionReviewChannelId;
  if (channelId && reviewId && channelId === reviewId) {
    return reply(errorEmbed('Same Channel', 'The review channel has to be a different channel from the one confessions are posted in.'));
  }

  let saved;
  try {
    saved = await updateSettings(guildId, update);
  } catch (err) {
    logger.error(`[confessions] ${guildId}: setup failed (${errTag(err)})`);
    return reply(TRY_AGAIN());
  }
  logger.info(`[confessions] ${guildId}: setup changed ${Object.keys(update).join(', ')} by ${interaction.user.id}`);

  const embed = configEmbed(saved, '✅ Confessions Updated');
  const warnings = [];
  if (resolved.review && everyoneCanView(resolved.review) !== false) {
    warnings.push(`⚠️ <#${resolved.review.id}> can be seen by @everyone, so members would read confessions before they are approved. Make it private.`);
  }
  if (saved.confessionReviewChannelId && !saved.confessionChannelId) warnings.push('⚠️ Pick the confession channel too, or approved confessions have nowhere to go.');
  if (!saved.confessionChannelId) warnings.push('⚠️ Pick a channel with the `channel` option, or nobody can confess.');
  if (warnings.length) embed.addFields({ name: 'Check', value: warnings.join('\n') });
  return reply(embed);
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'submit') return submit(interaction, client);
  // Everything else is staff: answered ephemerally.
  if (!(await safeDefer(interaction, { ephemeral: true }))) return undefined;
  if (sub === 'delete') return remove(interaction, client);
  if (sub === 'block') return block(interaction, true);
  if (sub === 'unblock') return block(interaction, false);
  if (sub === 'reveal') return reveal(interaction, client);
  if (sub === 'panel') return panel(interaction);
  if (sub === 'setup') return setup(interaction);
  return undefined;
}
