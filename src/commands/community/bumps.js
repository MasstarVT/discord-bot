// /bumps: bump reminders (beyond MEE6). The bot watches for a successful Disboard /bump, reminds the server when it can bump again
// and keeps a leaderboard. Members run `status` and `leaderboard`; admins run `setup` and `reset` (subcommandPermissions). The command
// is not gated, so admins can set the channel and role up before turning the module on; the member subcommands check the module
// themselves and refuse with the router's wording while it is off. It is `/bumps`, not `/bump`, so it never shadows Disboard's own
// /bump in the slash menu. The rules live in services/bumpService.js.
import { ChannelType, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  getSettingsSafe, isModuleEnabled, peekSettings, updateSettings,
} from '../../services/settingsService.js';
import {
  BUMP_SITES, bumpCountOf, getBumpStates, moduleOffEmbed, resetCounts, topBumpers,
} from '../../services/bumpService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  awaitConfirm, checkChannelUsable, pingableRoleIds, safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = 'bumps';
export const gated = false;                                  // `setup` and `reset` work while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER;     // status, leaderboard
export const subcommandPermissions = Object.freeze({
  setup: PERMISSION_LEVELS.ADMIN,
  reset: PERMISSION_LEVELS.ADMIN,
});
// No setDefaultMemberPermissions: members must see `status` and `leaderboard`; guild only.

export const data = new SlashCommandBuilder()
  .setName('bumps')
  .setDescription('Bump reminders and the bump leaderboard.')
  .addSubcommand((s) => s.setName('status').setDescription('When the server can be bumped again.'))
  .addSubcommand((s) => s.setName('leaderboard').setDescription('Who has bumped the server the most.'))
  .addSubcommand((s) => s.setName('setup').setDescription('Set where and how to remind the server (no options: show them).')
    .addChannelOption((o) => o.setName('channel').setDescription('Where reminders are posted')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
    .addRoleOption((o) => o.setName('role').setDescription('Role to ping when the server can be bumped'))
    .addBooleanOption((o) => o.setName('thanks').setDescription('Thank members right after they bump'))
    .addIntegerOption((o) => o.setName('coins').setDescription('Coins for each bump (0 = none)')
      .setMinValue(0).setMaxValue(100_000)))
  .addSubcommand((s) => s.setName('reset').setDescription('Clear bump counts.')
    .addUserOption((o) => o.setName('user').setDescription('Only this member (empty = everyone)')));

const NO_MENTIONS = Object.freeze({ parse: [] });
const MEDALS = Object.freeze(['🥇', '🥈', '🥉']);
const SETTINGS_UNAVAILABLE = () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const DB_ERROR = () => errorEmbed('Something Went Wrong', 'That could not be done right now. Please try again in a moment.');

const plural = (n, word) => `${n.toLocaleString('en-US')} ${word}${n === 1 ? '' : 's'}`;
const yesNo = (v) => (v ? 'Yes' : 'No');
const stamp = (date, style) => `<t:${Math.floor(new Date(date).getTime() / 1000)}:${style}>`;

// ── Gate for the member subcommands ───────────────────────────────────────────

/** Settings from the memo, else a lookup bounded by what is left of the 3 s acknowledgement window. null: not available in time. */
async function settingsInBudget(interaction) {
  const hit = peekSettings(interaction.guildId);
  if (hit) return hit;
  const ms = ackBudgetMs(interaction);
  if (ms <= 0) return null;
  return withTimeout(getSettingsSafe(interaction.guildId), ms, 'bumps settings').catch(() => null);
}

/** @returns {Promise<object|null>} the settings, or null after replying (ephemerally) that the module is off or settings are missing */
async function memberGate(interaction) {
  const settings = await settingsInBudget(interaction);
  if (!settings) {
    await safeReply(interaction, { embeds: [SETTINGS_UNAVAILABLE()], ephemeral: true });
    return null;
  }
  if (!isModuleEnabled(settings, 'bumps')) {
    await safeReply(interaction, { embeds: [moduleOffEmbed()], ephemeral: true });
    return null;
  }
  return settings;
}

// ── status ────────────────────────────────────────────────────────────────────

async function status(interaction) {
  const settings = await memberGate(interaction);
  if (!settings) return;
  if (!(await safeDefer(interaction))) return;
  const reply = (payload) => safeReply(interaction, { ...payload, allowedMentions: NO_MENTIONS });

  let states;
  try {
    states = await getBumpStates(interaction.guildId);
  } catch (err) {
    logger.error(`[bumps] ${interaction.guildId}: status failed`, err);
    return reply({ embeds: [DB_ERROR()] });
  }
  const embed = guildEmbed(settings, '📣 Bump status');
  const byKey = new Map(states.map((s) => [s.site, s]));
  const now = Date.now();
  for (const [key, site] of Object.entries(BUMP_SITES)) {
    const s = byKey.get(key);
    if (!s) {
      embed.addFields({ name: site.name, value: `No bump seen yet. Use \`/bump\` with ${site.name}; I'll count it and remind you in time.` });
      continue;
    }
    const ready = new Date(s.nextBumpAt).getTime() <= now;
    embed.addFields({
      name: site.name,
      value: [
        `Last bump: ${stamp(s.lastBumpAt, 'R')}${s.lastBumperId ? ` by <@${s.lastBumperId}>` : ''}`,
        `Next bump: ${ready ? '**now**, the server can be bumped!' : `${stamp(s.nextBumpAt, 'R')} (${stamp(s.nextBumpAt, 't')})`}`,
      ].join('\n'),
    });
  }
  return reply({ embeds: [embed] });
}

// ── leaderboard ───────────────────────────────────────────────────────────────

async function leaderboard(interaction) {
  const settings = await memberGate(interaction);
  if (!settings) return;
  if (!(await safeDefer(interaction))) return;
  const reply = (payload) => safeReply(interaction, { ...payload, allowedMentions: NO_MENTIONS });

  let rows;
  let own = 0;
  try {
    rows = await topBumpers(interaction.guildId);
    if (!rows.some((r) => r.userId === interaction.user.id)) own = await bumpCountOf(interaction.guildId, interaction.user.id);
  } catch (err) {
    logger.error(`[bumps] ${interaction.guildId}: leaderboard failed`, err);
    return reply({ embeds: [DB_ERROR()] });
  }
  if (!rows.length) {
    return reply({ embeds: [guildEmbed(settings, '🏆 Bump leaderboard', 'Nobody has bumped yet. Be the first with `/bump`!')] });
  }
  const lines = rows.map((r, i) => `${MEDALS[i] ?? `**${i + 1}.**`} <@${r.userId}> — ${plural(r.count, 'bump')}`);
  const embed = guildEmbed(settings, '🏆 Bump leaderboard', lines.join('\n'));
  if (own > 0) embed.setFooter({ text: `You: ${plural(own, 'bump')}` });
  return reply({ embeds: [embed] });
}

// ── setup ─────────────────────────────────────────────────────────────────────

function configEmbed(settings, title) {
  const coins = settings.bumpRewardCoins ?? 0;
  return guildEmbed(settings, title, [
    `Module: ${isModuleEnabled(settings, 'bumps') ? '✅ On' : '❌ Off, turn it on with `/module enable bumps`'}`,
    `Reminder channel: ${settings.bumpChannelId ? `<#${settings.bumpChannelId}>` : 'the channel of the last bump'}`,
    `Role to ping: ${settings.bumpRoleId ? `<@&${settings.bumpRoleId}>` : 'none'}`,
    `Thank the bumper: ${yesNo(settings.bumpThanks)}`,
    `Coins per bump: ${coins > 0 ? coins.toLocaleString('en-US') : 'none'}`,
  ].join('\n'));
}

async function setup(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guild, guildId, options } = interaction;

  const settings = await getSettingsSafe(guildId);
  if (!settings) return reply(SETTINGS_UNAVAILABLE());

  const update = {};
  const picked = options.getChannel('channel');
  const role = options.getRole('role');
  const thanks = options.getBoolean('thanks');
  const coins = options.getInteger('coins');
  if (thanks !== null) update.bumpThanks = thanks;
  if (coins !== null) update.bumpRewardCoins = coins;

  let channel = null;
  if (picked) {
    channel = guild.channels.cache.get(picked.id) ?? null;
    const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
    if (!usable.ok) return reply(errorEmbed('Cannot Use That Channel', `${usable.reason} Pick another channel, or fix my permissions there.`));
    update.bumpChannelId = channel.id;
  }
  if (role) {
    if (role.id === guild.id) return reply(errorEmbed('Cannot Ping @everyone', 'Pick a role made for bump reminders instead of @everyone.'));
    update.bumpRoleId = role.id;
  }
  if (!Object.keys(update).length) return reply(configEmbed(settings, '📣 Bump Reminders'));

  let saved;
  try {
    saved = await updateSettings(guildId, update);
  } catch (err) {
    logger.error(`[bumps] ${guildId}: setup failed`, err);
    return reply(errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'));
  }
  logger.info(`[bumps] ${guildId}: setup changed ${Object.keys(update).join(', ')} by ${interaction.user.id}`);

  const embed = configEmbed(saved, '✅ Bump Reminders Updated');
  const target = (saved.bumpChannelId ? guild.channels.cache.get(saved.bumpChannelId) : null) ?? interaction.channel;
  if (saved.bumpRoleId && target) {
    const { allowed } = pingableRoleIds(target, guild.members.me, [saved.bumpRoleId]);
    if (!allowed.length) {
      embed.addFields({
        name: 'Role',
        value: `⚠️ I can't ping <@&${saved.bumpRoleId}>: it isn't mentionable and I don't have **Mention @everyone, @here and All Roles**. Make the role mentionable, or give me that permission.`,
      });
    }
  }
  if ((saved.bumpRewardCoins ?? 0) > 0 && !isModuleEnabled(saved, 'economy')) {
    embed.addFields({ name: 'Coins', value: '⚠️ The Economy module is off, so no coins are given. Turn it on with `/module enable economy`.' });
  }
  return reply(embed);
}

// ── reset ─────────────────────────────────────────────────────────────────────

async function reset(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guildId } = interaction;
  const user = interaction.options.getUser('user');

  if (!user) {
    const yes = await awaitConfirm(interaction, {
      title: 'Clear all bump counts?',
      description: 'This removes every member\'s bump count and empties the leaderboard. Reminders keep working. It cannot be undone.',
      confirmLabel: 'Clear all',
    });
    if (!yes) return undefined;
  }
  let removed;
  try {
    removed = await resetCounts(guildId, user?.id ?? null);
  } catch (err) {
    logger.error(`[bumps] ${guildId}: reset failed`, err);
    return reply(DB_ERROR());
  }
  logger.info(`[bumps] ${guildId}: counts of ${user?.id ?? 'everyone'} cleared by ${interaction.user.id}`);
  if (user) {
    return reply(successEmbed('Bump Count Cleared', removed ? `The bump count of <@${user.id}> was cleared.` : `<@${user.id}> had no bumps counted.`));
  }
  return reply(successEmbed('Bump Counts Cleared', removed ? `Cleared the counts of ${plural(removed, 'member')}.` : 'There were no bump counts to clear.'));
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'status') return status(interaction);
  if (sub === 'leaderboard') return leaderboard(interaction);
  if (sub === 'setup') return setup(interaction);
  if (sub === 'reset') return reset(interaction);
  return undefined;
}
