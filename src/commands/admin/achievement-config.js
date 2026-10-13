// /achievement-config (pkg-achievements.md §4.2): create, edit, delete, enable, disable, list, info, reset (MEE6
// /achievements-reset), preset, tier add|edit|remove and announce mode|channel|message. Admin config command: replies
// are public (CLAUDE.md admin convention) except the delete/reset prompts, and it keeps working while the Achievements
// module is off (gated = false). Business logic and database access live in achievementService.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { successEmbed, errorEmbed, infoEmbed, applyGuildColor } from '../../utils/embedBuilder.js';
import {
  safeDefer, safeReply, respondAutocomplete, checkChannelUsable, checkAssignableRole, permissionNames, awaitConfirm,
} from '../../utils/interactionHelpers.js';
import { getSettings, updateSettings } from '../../services/settingsService.js';
import { renderPlaceholdersDetailed } from '../../utils/placeholderParser.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { clampText, formatNumber, joinWithLimit } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';
import { paginate } from '../../utils/paginator.js';
import logger from '../../utils/logger.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import {
  TRIGGER_LABELS, TRIGGER_EMOJI, TIER_NAMES, TIER_EMOJIS, GOAL_MAX, GOAL_INPUT_MAX, MAX_REWARD_AMOUNT, PRESETS,
  DEFAULT_ACHIEVEMENT_MESSAGE, MODULE_OFF_NOTE, LIST_PAGE_SIZE,
  normalizeTiers, normalizeRewards, validateTiers, serializeTiers, mergeRewards, parseGoals, normalizeAchievementName,
  normalizeBadgeEmoji, parseDeadline, rewardUnavailableMessage, formatRewards, badgeOf, goalText, achievementStatus, sampleVars,
  withConfigLock, refreshAchievementCache, resolveAchievement, nameTaken, searchAchievements, createAchievement,
  updateAchievement, deleteAchievement, listAchievements, getTierStats, resetMemberProgress, applyTierChange,
} from '../../services/achievementService.js';

export const module = 'achievements';
export const gated = false;                              // config works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;  // every subcommand; no subcommandPermissions

const TEXT_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const ach = (o) => o.setName('achievement').setDescription('Achievement').setRequired(true).setAutocomplete(true).setMaxLength(100);
const rewardOpts = (s) => s
  .addRoleOption((o) => o.setName('role_add').setDescription('Reward: role to give'))
  .addStringOption((o) => newRoleOption(o, 'role_add'))
  .addRoleOption((o) => o.setName('role_remove').setDescription('Reward: role to take away'))
  .addIntegerOption((o) => o.setName('xp').setDescription('Reward: XP (needs Levels)').setMinValue(1).setMaxValue(MAX_REWARD_AMOUNT))
  .addIntegerOption((o) => o.setName('coins').setDescription('Reward: coins (needs Economy)').setMinValue(1).setMaxValue(MAX_REWARD_AMOUNT));
const TRIGGER_CHOICES = [
  { name: 'Messages sent', value: 'MESSAGES' },
  { name: 'Reactions added', value: 'REACTIONS' },
  { name: 'Level reached', value: 'LEVEL' },
  { name: 'Server boosts', value: 'BOOST' },
  { name: 'Shop items bought', value: 'PURCHASES' },
];

export const data = new SlashCommandBuilder()
  .setName('achievement-config')
  .setDescription('Set up achievements (MEE6 Achievements, /achievements-reset)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => rewardOpts(s.setName('create').setDescription('Create an achievement; rewards apply at every tier')
    .addStringOption((o) => o.setName('name').setDescription('Achievement name').setRequired(true).setMaxLength(50))
    .addStringOption((o) => o.setName('trigger').setDescription('What members do to progress').setRequired(true).addChoices(...TRIGGER_CHOICES))
    .addStringOption((o) => o.setName('goals').setDescription('Goal per tier, low to high: 100 or 100,500,1k (max 4)').setRequired(true).setMaxLength(60)))
    .addStringOption((o) => o.setName('description').setDescription('Short description').setMaxLength(200))
    .addStringOption((o) => o.setName('emoji').setDescription('Badge emoji').setMaxLength(64))
    .addBooleanOption((o) => o.setName('track_past').setDescription('Count past activity (default True)'))
    .addStringOption((o) => o.setName('deadline').setDescription('Last moment to progress, e.g. 2026-12-31 23:59').setMaxLength(40)))
  .addSubcommand((s) => s.setName('edit').setDescription('Change name, description, badge or deadline').addStringOption(ach)
    .addStringOption((o) => o.setName('name').setDescription('New name').setMaxLength(50))
    .addStringOption((o) => o.setName('description').setDescription('New description ("none" to clear)').setMaxLength(200))
    .addStringOption((o) => o.setName('emoji').setDescription('New badge emoji ("none" to clear)').setMaxLength(64))
    .addBooleanOption((o) => o.setName('track_past').setDescription('Count past activity'))
    .addStringOption((o) => o.setName('deadline').setDescription('New deadline ("none" to clear)').setMaxLength(40)))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete an achievement and its progress').addStringOption(ach))
  .addSubcommand((s) => s.setName('enable').setDescription('Turn an achievement on').addStringOption(ach))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn an achievement off (progress is kept)').addStringOption(ach))
  .addSubcommand((s) => s.setName('list').setDescription('List achievements'))
  .addSubcommand((s) => s.setName('info').setDescription('Show tiers, rewards and unlock counts').addStringOption(ach))
  .addSubcommand((s) => s.setName('reset').setDescription("Reset a member's progress (MEE6 /achievements-reset)")
    .addUserOption((o) => o.setName('member').setDescription('Member to reset').setRequired(true))
    .addStringOption((o) => o.setName('achievement').setDescription('Only this achievement (default: all)').setAutocomplete(true).setMaxLength(100)))
  .addSubcommand((s) => s.setName('preset').setDescription('Add a pre-made achievement')
    .addStringOption((o) => o.setName('template').setDescription('Pre-made achievement').setRequired(true).addChoices(
      { name: 'Chatterbox (messages)', value: 'chatterbox' },
      { name: 'Booster (boosts)', value: 'booster' },
      { name: 'Level 10 (level)', value: 'level10' }))
    .addStringOption((o) => o.setName('name').setDescription('Custom name').setMaxLength(50)))
  .addSubcommandGroup((g) => g.setName('tier').setDescription('Manage tiers (Bronze, Silver, Gold, Diamond)')
    .addSubcommand((s) => rewardOpts(s.setName('add').setDescription('Add a tier above the current top tier').addStringOption(ach)
      .addIntegerOption((o) => o.setName('goal').setDescription('Goal for the new tier').setRequired(true).setMinValue(1).setMaxValue(GOAL_INPUT_MAX))))
    .addSubcommand((s) => rewardOpts(s.setName('edit').setDescription("Change a tier's goal or rewards").addStringOption(ach)
      .addIntegerOption((o) => o.setName('tier').setDescription('Tier number (1 = Bronze)').setRequired(true).setMinValue(1).setMaxValue(LIMITS.ACHIEVEMENT_TIERS))
      .addIntegerOption((o) => o.setName('goal').setDescription('New goal').setMinValue(1).setMaxValue(GOAL_INPUT_MAX)))
      .addBooleanOption((o) => o.setName('clear_rewards').setDescription('Remove existing rewards first')))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a tier').addStringOption(ach)
      .addIntegerOption((o) => o.setName('tier').setDescription('Tier number (1 = Bronze)').setRequired(true).setMinValue(1).setMaxValue(LIMITS.ACHIEVEMENT_TIERS))))
  .addSubcommandGroup((g) => g.setName('announce').setDescription('Unlock announcements')
    .addSubcommand((s) => s.setName('mode').setDescription('Off, current channel, DM or a set channel')
      .addStringOption((o) => o.setName('mode').setDescription('Where to announce').setRequired(true).addChoices(
        { name: 'Disabled', value: 'OFF' }, { name: 'Current channel', value: 'CURRENT' },
        { name: 'Private message', value: 'DM' }, { name: 'Custom channel', value: 'CHANNEL' }))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel for Custom channel mode').addChannelTypes(...TEXT_TYPES))
      .addStringOption((o) => newChannelOption(o))
      .addBooleanOption((o) => o.setName('embed').setDescription('Send unlocks as an embed')))
    .addSubcommand((s) => s.setName('channel').setDescription('Set the announcement channel')
      .addChannelOption((o) => o.setName('channel').setDescription('Channel to announce in').addChannelTypes(...TEXT_TYPES))
      .addStringOption((o) => newChannelOption(o)))
    .addSubcommand((s) => s.setName('message').setDescription('Set the announcement text (blank = default)')
      .addStringOption((o) => o.setName('text').setDescription('Text with {player} {achievement} {tier} {rewards}').setMaxLength(1500))));

const MODE_LABELS = Object.freeze({ OFF: 'Disabled', CURRENT: 'Current channel', DM: 'Private message', CHANNEL: 'Custom channel' });
const NOT_FOUND = ['Achievement Not Found', "I couldn't find that achievement. Pick one from the list."];
const GENERIC_ERROR = ['Something Went Wrong', 'I could not save that. Please try again in a moment.'];

// ── Shared validation helpers ────────────────────────────────────────────────────────────────────────────

const fail = (title, error) => ({ ok: false, title, error });

/**
 * Reads the four reward options and checks them against the guild.
 * @returns {{ ok: true, rewards: object[], given: Set<string> } | { ok: false, title: string, error: string }}
 */
function collectRewards(interaction, settings) {
  const guild = interaction.guild;
  const o = interaction.options;
  const roleAdd = o.getRole('role_add');
  const roleRemove = o.getRole('role_remove');
  const xp = o.getInteger('xp');
  const coins = o.getInteger('coins');
  if (roleAdd && roleRemove && roleAdd.id === roleRemove.id) return fail('Invalid Rewards', 'A tier cannot give and take the same role.');
  if (roleAdd) {
    const r = checkAssignableRole(guild, guild.roles.cache.get(roleAdd.id) ?? roleAdd, { executor: interaction.member });
    if (!r.ok) return fail('Role Not Allowed', `<@&${roleAdd.id}>: ${r.reason}`);
  }
  if (roleRemove) {
    const r = checkAssignableRole(guild, guild.roles.cache.get(roleRemove.id) ?? roleRemove, { executor: interaction.member, allowDangerous: true });
    if (!r.ok) return fail('Role Not Allowed', `<@&${roleRemove.id}>: ${r.reason}`);
  }
  if (xp != null) {
    const msg = rewardUnavailableMessage('xp', settings);
    if (msg) return fail('Reward Unavailable', msg);
  }
  if (coins != null) {
    const msg = rewardUnavailableMessage('coins', settings);
    if (msg) return fail('Reward Unavailable', msg);
  }
  const given = new Set();
  const list = [];
  if (roleAdd) { given.add('ROLE_ADD'); list.push({ type: 'ROLE_ADD', roleId: roleAdd.id }); }
  if (roleRemove) { given.add('ROLE_REMOVE'); list.push({ type: 'ROLE_REMOVE', roleId: roleRemove.id }); }
  if (xp != null) { given.add('XP'); list.push({ type: 'XP', amount: xp }); }
  if (coins != null) { given.add('COINS'); list.push({ type: 'COINS', amount: coins }); }
  return { ok: true, rewards: normalizeRewards(list), given };
}

/** Warning lines (each starts with a newline) for a trigger whose backing module or channel is missing. */
function dependencyWarnings(trigger, settings) {
  let out = '';
  if (trigger === 'LEVEL' && !isModuleEnabled(settings, 'leveling')) out += '\n⚠️ Level achievements only progress while the ⭐ Levels module is on.';
  if (trigger === 'PURCHASES' && !isModuleEnabled(settings, 'economy')) out += '\n⚠️ Purchase achievements only progress while the 🪙 Economy module is on.';
  if (trigger === 'BOOST' && ['CURRENT', 'CHANNEL'].includes(settings?.achievementAnnounceMode ?? 'CURRENT') && !settings?.achievementChannelId) {
    out += '\nℹ️ Boosts happen outside any channel. Set `/achievement-config announce channel` to announce them.';
  }
  return out;
}

/** Multi-line description of an achievement row (create, edit, preset, enable, disable and tier replies). */
function achievementSummary(a, settings, guild) {
  const tiers = normalizeTiers(a.tiers);
  const lines = [`${badgeOf(a)} **${a.name}** · ${TRIGGER_LABELS[a.trigger]} · ${achievementStatus(a)}`];
  if (a.description) lines.push(`*${a.description}*`);
  tiers.forEach((t, i) => {
    lines.push(`${TIER_EMOJIS[i]} ${TIER_NAMES[i]} — ${goalText(a.trigger, t.goal)}${t.rewards.length ? ` → ${formatRewards(t.rewards, guild, settings)}` : ''}`);
  });
  lines.push(`Past activity: ${a.trackPastProgress ? 'counted' : 'not counted'} · Deadline: ${a.deadline ? discordTimestamp(a.deadline, 'f') : 'none'}`);
  return lines.join('\n');
}

const rewardCountOf = (a) => normalizeTiers(a.tiers).reduce((n, t) => n + t.rewards.length, 0);

/** Runs `fn` under the guild's config lock and refreshes the achievement index afterwards when it succeeded. */
async function locked(c, fn, { refresh = true } = {}) {
  return withConfigLock(c.guildId, async () => {
    const out = await fn();
    if (refresh && out?.ok) await refreshAchievementCache(c.guildId);
    return out;
  });
}

/** Sends a handler result: an error embed or the success embed. `followUp` is for results after a confirm prompt. */
async function send(c, out, { followUp = false } = {}) {
  const payload = out.ok
    ? { embeds: [out.embed], allowedMentions: { parse: [] } }
    : { embeds: [errorEmbed(out.title, out.error)], allowedMentions: { parse: [] } };
  if (followUp) {
    await c.interaction.followUp({ ...payload, ephemeral: !out.ok });
  } else {
    await c.interaction.editReply(payload);
  }
}

const okEmbed = (c, title, description) => ({ ok: true, embed: successEmbed(title, description + (c.moduleOff ? MODULE_OFF_NOTE : '')) });

/** Validates and reads name, emoji, description and deadline options shared by create and edit. */
function parseCommon(c, { editing }) {
  const o = c.interaction.options;
  const out = {};
  const nameRaw = o.getString('name');
  if (nameRaw !== null) {
    const name = normalizeAchievementName(nameRaw);
    if (!name) return fail('Invalid Name', 'Names must be 1-50 characters.');
    out.name = name;
  }
  const emojiRaw = o.getString('emoji');
  if (emojiRaw !== null) {
    const e = normalizeBadgeEmoji(emojiRaw, { hasCustomEmoji: (id) => c.interaction.client.emojis.cache.has(id), allowClear: editing });
    if (!e.ok) return fail('Invalid Emoji', e.error);
    out.emoji = e.value;
  }
  const descRaw = o.getString('description');
  if (descRaw !== null) {
    const t = descRaw.trim();
    out.description = t === '' || (editing && t.toLowerCase() === 'none') ? null : t;
  }
  const deadlineRaw = o.getString('deadline');
  if (deadlineRaw !== null) {
    const d = parseDeadline(deadlineRaw, c.settings?.timezone ?? 'UTC', { allowClear: editing });
    if (!d.ok) return fail('Invalid Deadline', d.error);
    out.deadline = d.value;
  }
  const track = o.getBoolean('track_past');
  if (track !== null) out.trackPastProgress = track;
  return { ok: true, values: out };
}

// ── Handlers ─────────────────────────────────────────────────────────────────────────────────────────────

async function create(c) {
  const { interaction, settings, guildId } = c;
  const o = interaction.options;
  // Checks run in the spec's order (name, goals, emoji, description, deadline, rewards); the first failure replies.
  if (!normalizeAchievementName(o.getString('name', true))) return send(c, fail('Invalid Name', 'Names must be 1-50 characters.'));
  const trigger = o.getString('trigger', true);
  const g = parseGoals(o.getString('goals', true), trigger);
  if (!g.ok) return send(c, fail('Invalid Goals', g.error));
  const common = parseCommon(c, { editing: false });
  if (!common.ok) return send(c, common);
  const { name, emoji = null, description = null, deadline = null, trackPastProgress = true } = common.values;
  const rw = collectRewards(interaction, settings);
  if (!rw.ok) return send(c, rw);

  const out = await locked(c, async () => {
    const r = await createAchievement(guildId, {
      name, description, emoji, trigger, trackPastProgress, deadline, tiers: g.goals.map((goal) => ({ goal, rewards: rw.rewards })),
    });
    if (!r.ok) {
      return r.code === 'limit'
        ? fail('Limit Reached', `A server can have at most ${LIMITS.ACHIEVEMENTS} achievements. Delete one first.`)
        : fail('Name Taken', `An achievement named **${name}** already exists.`);
    }
    const extra = rw.rewards.length && g.goals.length > 1
      ? '\nℹ️ These rewards are given at every tier. Use `/achievement-config tier edit` for per-tier rewards.' : '';
    return okEmbed(c, 'Achievement Created', achievementSummary(r.achievement, settings, interaction.guild) + dependencyWarnings(trigger, settings) + extra);
  });
  return send(c, out);
}

async function edit(c) {
  const { interaction, settings, guildId } = c;
  const o = interaction.options;
  const out = await locked(c, async () => {
    const a = await resolveAchievement(guildId, o.getString('achievement', true));
    if (!a) return fail(...NOT_FOUND);
    if (!['name', 'description', 'emoji', 'track_past', 'deadline'].some((n) => o.get(n) != null)) {
      return fail('Nothing To Change', 'Pass at least one option to change.');
    }
    const common = parseCommon(c, { editing: true });
    if (!common.ok) return common;
    const v = common.values;
    if (v.name !== undefined && await nameTaken(guildId, v.name, a.id)) return fail('Name Taken', `An achievement named **${v.name}** already exists.`);
    const r = await updateAchievement(guildId, a.id, v);
    if (!r.ok) return r.code === 'name-taken' ? fail('Name Taken', `An achievement named **${v.name}** already exists.`) : fail(...NOT_FOUND);

    const changes = [];
    if (v.name !== undefined) changes.push(`• **Name:** ${a.name} → ${v.name}`);
    if (v.description !== undefined) changes.push(`• **Description:** ${a.description ?? 'none'} → ${v.description ?? 'none'}`);
    if (v.emoji !== undefined) changes.push(`• **Badge:** ${a.emoji ?? 'default'} → ${v.emoji ?? 'default'}`);
    if (v.trackPastProgress !== undefined) changes.push(`• **Past activity:** ${a.trackPastProgress ? 'counted' : 'not counted'} → ${v.trackPastProgress ? 'counted' : 'not counted'}`);
    if (v.deadline !== undefined) {
      changes.push(`• **Deadline:** ${a.deadline ? discordTimestamp(a.deadline, 'f') : 'none'} → ${v.deadline ? discordTimestamp(v.deadline, 'f') : 'none'}`);
    }
    let text = `${changes.join('\n')}\n\n${achievementSummary({ ...a, ...v }, settings, interaction.guild)}`;
    if (v.trackPastProgress !== undefined) text += '\nℹ️ This applies to members who have no progress here yet; existing progress is kept.';
    return okEmbed(c, 'Achievement Updated', text);
  });
  return send(c, out);
}

async function remove(c) {
  const { interaction, guildId } = c;
  const a = await resolveAchievement(guildId, interaction.options.getString('achievement', true));
  if (!a) return send(c, fail(...NOT_FOUND));
  const yes = await awaitConfirm(interaction, {
    title: 'Delete achievement?',
    description: `Delete **${a.name}** and every member's progress on it? Roles, XP and coins already given are kept.`,
    confirmLabel: 'Delete',
  });
  if (!yes) return undefined;
  const out = await locked(c, async () => {
    const count = await deleteAchievement(guildId, a.id);
    return count === 0 ? fail('Not Found', 'That achievement was already deleted.') : okEmbed(c, 'Achievement Deleted', `**${a.name}** was deleted.`);
  });
  return send(c, out, { followUp: true });
}

async function toggle(c, enabled) {
  const { interaction, settings, guildId } = c;
  const out = await locked(c, async () => {
    const a = await resolveAchievement(guildId, interaction.options.getString('achievement', true));
    if (!a) return fail(...NOT_FOUND);
    if (a.enabled === enabled) return { ok: true, embed: infoEmbed('No Change', `**${a.name}** is already ${enabled ? 'on' : 'off'}.`) };
    const r = await updateAchievement(guildId, a.id, { enabled });
    if (!r.ok) return fail(...NOT_FOUND);
    let text = achievementSummary({ ...a, enabled }, settings, interaction.guild);
    if (enabled) {
      text += dependencyWarnings(a.trigger, settings);
      if (a.deadline && new Date(a.deadline).getTime() <= Date.now()) {
        text += '\n⌛ Its deadline has passed, so nothing is counted. Change it with /achievement-config edit.';
      }
    } else {
      text += '\nProgress is kept, and nothing is counted until you turn it back on.';
    }
    return okEmbed(c, enabled ? 'Achievement Enabled' : 'Achievement Disabled', text);
  });
  return send(c, out);
}

function announceLine(settings, guild) {
  const mode = settings?.achievementAnnounceMode ?? 'CURRENT';
  const id = settings?.achievementChannelId ?? null;
  let line = `Announcements: ${MODE_LABELS[mode] ?? mode}`;
  if (mode === 'CHANNEL' && id) line += ` <#${id}>`;
  if (id && !guild.channels.cache.has(id)) line += ' ⚠️ channel missing';
  return `${line} · Format: ${settings?.achievementUseEmbed ? 'embed' : 'plain text'}`;
}

async function list(c) {
  const { interaction, settings, guildId } = c;
  const rows = await listAchievements(guildId);
  if (rows.length === 0) {
    return interaction.editReply({ embeds: [applyGuildColor(infoEmbed('Achievements', 'No achievements yet. Try `/achievement-config preset` or `/achievement-config create`.'), settings)] });
  }
  const now = Date.now();
  const lines = rows.map((a) => {
    const goals = normalizeTiers(a.tiers).map((t) => formatNumber(t.goal)).join(' → ');
    const status = achievementStatus(a, now);
    const rewardCount = rewardCountOf(a);
    return `${badgeOf(a)} **${a.name}** · ${TRIGGER_LABELS[a.trigger]} · ${goals} · ${{ on: '✅ on', off: '⏸️ off', ended: '⌛ ended' }[status]}${rewardCount ? ` · 🎁 ${rewardCount}` : ''}`;
  });
  const pages = [];
  for (let i = 0; i < lines.length; i += LIST_PAGE_SIZE) {
    let body = `${rows.length}/${LIMITS.ACHIEVEMENTS} achievements\n\n${lines.slice(i, i + LIST_PAGE_SIZE).join('\n')}`;
    if (i === 0) body += `\n\n${announceLine(settings, interaction.guild)}`;
    pages.push(applyGuildColor(infoEmbed('Achievements', clampText(body, 4096)), settings));
  }
  return paginate(interaction, pages);
}

async function info(c) {
  const { interaction, settings, guildId } = c;
  const a = await resolveAchievement(guildId, interaction.options.getString('achievement', true));
  if (!a) return send(c, fail(...NOT_FOUND));
  const tiers = normalizeTiers(a.tiers);
  const { perTier, total } = await getTierStats(a.id, tiers.length);
  const header = [
    a.description ? `*${a.description}*` : null,
    `Trigger: ${TRIGGER_LABELS[a.trigger]} · Status: ${achievementStatus(a)}`,
    `Past activity: ${a.trackPastProgress ? 'counted' : 'not counted'} · Deadline: ${a.deadline ? discordTimestamp(a.deadline, 'f') : 'none'}`,
    `Members with progress: ${formatNumber(total)}`,
  ].filter(Boolean).join('\n');
  const embed = infoEmbed(`${badgeOf(a)} ${a.name}`, header);
  tiers.forEach((t, i) => {
    embed.addFields({
      name: clampText(`${TIER_EMOJIS[i]} ${TIER_NAMES[i]} — ${goalText(a.trigger, t.goal)}`, 256),
      value: clampText(`${formatRewards(t.rewards, interaction.guild, settings) || 'No rewards'}\nUnlocked by ${formatNumber(perTier[i])} member(s)`, 1024),
    });
  });
  applyGuildColor(embed, settings);
  return interaction.editReply({ embeds: [embed], allowedMentions: { parse: [] } });
}

async function reset(c) {
  const { interaction, guildId } = c;
  const user = interaction.options.getUser('member', true);
  if (user.bot) return send(c, fail('Not Available', "Bots don't earn achievements."));
  const input = interaction.options.getString('achievement');
  let scope;
  if (input) {
    const a = await resolveAchievement(guildId, input);
    if (!a) return send(c, fail(...NOT_FOUND));
    scope = [a];
  } else {
    scope = await listAchievements(guildId);
    if (scope.length === 0) return interaction.editReply({ embeds: [infoEmbed('Nothing To Reset', 'This server has no achievements.')] });
  }
  const label = scope.length === 1 ? `**${scope[0].name}**` : `all ${scope.length} achievements`;
  const yes = await awaitConfirm(interaction, {
    title: 'Reset achievement progress?',
    description: `Reset ${user}'s progress on ${label}? Roles, XP and coins already given are **kept**.`,
    confirmLabel: 'Reset',
  });
  if (!yes) return undefined;
  // A config write (serialised with the others of this guild) that does not change the index, so no cache refresh.
  await withConfigLock(guildId, () => resetMemberProgress(guildId, user.id, scope.map((a) => a.id)));
  const levelNote = scope.some((a) => a.trigger === 'LEVEL') ? '\nℹ️ Level achievements unlock again when the member next levels up.' : '';
  return send(c, okEmbed(c, 'Progress Reset', `${user}'s progress on ${label} was reset to zero. Rewards already given were kept.${levelNote}`), { followUp: true });
}

async function preset(c) {
  const { interaction, settings, guildId } = c;
  const p = PRESETS[interaction.options.getString('template', true)];
  if (!p) return send(c, fail('Unknown Template', 'Pick one of the pre-made achievements.'));
  const name = normalizeAchievementName(interaction.options.getString('name') ?? p.name);
  if (!name) return send(c, fail('Invalid Name', 'Names must be 1-50 characters.'));
  const out = await locked(c, async () => {
    const r = await createAchievement(guildId, {
      name, description: p.description, emoji: p.emoji, trigger: p.trigger, trackPastProgress: true, deadline: null,
      tiers: p.goals.map((goal) => ({ goal, rewards: [] })),
    });
    if (!r.ok) {
      return r.code === 'limit'
        ? fail('Limit Reached', `A server can have at most ${LIMITS.ACHIEVEMENTS} achievements. Delete one first.`)
        : fail('Name Taken', `An achievement named **${name}** already exists. Pass \`name:\` to add it under another name.`);
    }
    return okEmbed(c, 'Achievement Added',
      `${achievementSummary(r.achievement, settings, interaction.guild)}\nℹ️ Add rewards with \`/achievement-config tier edit\`.${dependencyWarnings(p.trigger, settings)}`);
  });
  return send(c, out);
}

async function tierAdd(c) {
  const { interaction, settings, guildId } = c;
  const goal = interaction.options.getInteger('goal', true);
  const out = await locked(c, async () => {
    const a = await resolveAchievement(guildId, interaction.options.getString('achievement', true));
    if (!a) return fail(...NOT_FOUND);
    const tiers = normalizeTiers(a.tiers);
    if (tiers.length >= LIMITS.ACHIEVEMENT_TIERS) return fail('Too Many Tiers', `An achievement can have at most ${LIMITS.ACHIEVEMENT_TIERS} tiers.`);
    const last = tiers.at(-1).goal;
    if (goal <= last) return fail('Invalid Goal', `The new goal must be higher than the top tier's goal (${formatNumber(last)}).`);
    if (goal > GOAL_MAX[a.trigger]) return fail('Invalid Goal', `${TRIGGER_LABELS[a.trigger]} goals can be at most ${formatNumber(GOAL_MAX[a.trigger])}.`);
    const rw = collectRewards(interaction, settings);
    if (!rw.ok) return rw;
    const next = serializeTiers([...tiers, { goal, rewards: rw.rewards }]);
    const v = validateTiers(next, a.trigger);
    if (!v.ok) return fail('Invalid Goal', v.error);
    await applyTierChange(a.id, next, { kind: 'add' });
    const n = next.length - 1;
    return okEmbed(c, 'Tier Added',
      `${TIER_EMOJIS[n]} **${TIER_NAMES[n]}** added to **${a.name}**.\n\n${achievementSummary({ ...a, tiers: next }, settings, interaction.guild)}\nMembers who already passed this goal unlock it with their next activity.`);
  });
  return send(c, out);
}

async function tierEdit(c) {
  const { interaction, settings, guildId } = c;
  const o = interaction.options;
  const tierNum = o.getInteger('tier', true);
  const goal = o.getInteger('goal');
  const clear = o.getBoolean('clear_rewards') === true;
  const rewardGiven = ['role_add', 'role_remove', 'xp', 'coins'].some((n) => o.get(n) != null);
  const out = await locked(c, async () => {
    const a = await resolveAchievement(guildId, o.getString('achievement', true));
    if (!a) return fail(...NOT_FOUND);
    const tiers = normalizeTiers(a.tiers);
    const k = tierNum - 1;
    if (k >= tiers.length) return fail('Unknown Tier', `**${a.name}** has only ${tiers.length} tier(s).`);
    if (goal == null && !rewardGiven && !clear) return fail('Nothing To Change', 'Pass at least one option to change.');
    if (goal != null) {
      if (goal > GOAL_MAX[a.trigger]) return fail('Invalid Goal', `${TRIGGER_LABELS[a.trigger]} goals can be at most ${formatNumber(GOAL_MAX[a.trigger])}.`);
      const prev = tiers[k - 1]?.goal ?? 0;
      const nxt = tiers[k + 1]?.goal ?? Infinity;
      if (!(goal > prev && goal < nxt)) {
        return fail('Invalid Goal', `Tier ${tierNum} must be above ${formatNumber(prev)}${tiers[k + 1] ? ` and below ${formatNumber(nxt)}` : ''}.`);
      }
    }
    const rw = collectRewards(interaction, settings);
    if (!rw.ok) return rw;
    const rewards = mergeRewards(clear ? [] : tiers[k].rewards, rw.rewards, rw.given);
    const next = serializeTiers(tiers.map((t, i) => (i === k ? { goal: goal ?? t.goal, rewards } : { goal: t.goal, rewards: t.rewards })));
    const v = validateTiers(next, a.trigger);
    if (!v.ok) return fail('Invalid Goal', v.error);
    const goalChanged = goal != null && goal !== tiers[k].goal;
    await applyTierChange(a.id, next, { kind: 'edit', index: k, goalChanged });
    let text = achievementSummary({ ...a, tiers: next }, settings, interaction.guild);
    if (goalChanged) {
      text += goal < tiers[k].goal
        ? '\nMembers who already passed the new goal unlock it with their next activity.'
        : '\nMembers who already unlocked this tier keep it.';
    }
    return okEmbed(c, 'Tier Updated', text);
  });
  return send(c, out);
}

async function tierRemove(c) {
  const { interaction, settings, guildId } = c;
  const tierNum = interaction.options.getInteger('tier', true);
  const out = await locked(c, async () => {
    const a = await resolveAchievement(guildId, interaction.options.getString('achievement', true));
    if (!a) return fail(...NOT_FOUND);
    const tiers = normalizeTiers(a.tiers);
    const k = tierNum - 1;
    if (k >= tiers.length) return fail('Unknown Tier', `**${a.name}** has only ${tiers.length} tier(s).`);
    if (tiers.length === 1) return fail('Last Tier', 'An achievement needs at least one tier. Delete it with `/achievement-config delete` instead.');
    const next = serializeTiers(tiers.filter((_, i) => i !== k));
    await applyTierChange(a.id, next, { kind: 'remove', index: k });
    return okEmbed(c, 'Tier Removed', `${achievementSummary({ ...a, tiers: next }, settings, interaction.guild)}\nRewards already given for that tier were kept.`);
  });
  return send(c, out);
}

/** The bot must be able to post in `chan`; embed mode also needs EmbedLinks. Null when fine. */
function channelProblem(chan, embedOn) {
  const u = checkChannelUsable(chan, { needs: embedOn ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages'] });
  if (u.ok) return null;
  return fail('Missing Permissions', u.missing.length ? `I need **${permissionNames(u.missing).join(', ')}** in ${chan} to announce there.` : u.reason);
}

async function announceMode(c) {
  const { interaction, settings, guildId } = c;
  const o = interaction.options;
  const mode = o.getString('mode', true);
  const optCh = o.getChannel('channel');
  const embed = o.getBoolean('embed');
  const guild = interaction.guild;
  const optChannel = optCh ? (guild.channels.cache.get(optCh.id) ?? optCh) : null;
  // A stored id that is no longer in the cache counts as no channel (display only; nothing is deleted).
  const stored = settings.achievementChannelId ? guild.channels.cache.get(settings.achievementChannelId) ?? null : null;
  const chan = optChannel ?? stored;
  if (mode === 'CHANNEL' && !chan) return send(c, fail('Pick a Channel', 'Custom channel mode needs a channel. Run this again with `channel:`.'));
  const embedOn = embed ?? settings.achievementUseEmbed === true;
  if (mode === 'CHANNEL' || optChannel) {
    const problem = channelProblem(chan, embedOn);
    if (problem) return send(c, problem);
  }
  await updateSettings(guildId, {
    achievementAnnounceMode: mode,
    ...(optChannel ? { achievementChannelId: optChannel.id } : {}),
    ...(embed !== null ? { achievementUseEmbed: embed } : {}),
  });
  const channelId = optChannel?.id ?? settings.achievementChannelId ?? null;
  let line;
  if (mode === 'OFF') line = 'Unlocks are no longer announced.';
  else if (mode === 'CURRENT') {
    line = 'Unlocks are announced in the channel where they happen.';
    line += channelId
      ? `\nEvents without a channel (boosts, purchases) use <#${channelId}>.`
      : '\nEvents without a channel (boosts, purchases) are not announced unless you set `/achievement-config announce channel`.';
  } else if (mode === 'DM') {
    line = 'Members get a private message when they unlock a tier, and one "almost there" message at 90% of their next goal.\nMembers who block DMs from this server get nothing.';
  } else {
    line = `Unlocks are announced in <#${channelId}>.`;
  }
  if (embed !== null && mode !== 'OFF') line += `\nUnlocks are sent as ${embed ? 'an embed' : 'plain text'}.`;
  else if (embed !== null) line += `\nWhen turned on again, unlocks are sent as ${embed ? 'an embed' : 'plain text'}.`;
  return send(c, okEmbed(c, 'Announcements Updated', line));
}

async function announceChannel(c) {
  const { interaction, settings, guildId } = c;
  const channel = interaction.options.getChannel('channel', true);
  const resolved = interaction.guild.channels.cache.get(channel.id) ?? channel;
  const problem = channelProblem(resolved, settings.achievementUseEmbed === true);
  if (problem) return send(c, problem);
  const oldMode = settings.achievementAnnounceMode ?? 'CURRENT';
  await updateSettings(guildId, { achievementChannelId: channel.id, achievementAnnounceMode: 'CHANNEL' });
  return send(c, okEmbed(c, 'Announcements Updated',
    `Unlocks are announced in <#${channel.id}>.${oldMode !== 'CHANNEL' ? `\nMode switched from ${MODE_LABELS[oldMode]} to Custom channel.` : ''}`));
}

async function announceMessage(c) {
  const { interaction, settings, guildId } = c;
  const text = interaction.options.getString('text')?.trim();
  if (!text) {
    await updateSettings(guildId, { achievementMessage: null });
    return send(c, okEmbed(c, 'Announcement Message Reset', `Using the default: ${DEFAULT_ACHIEVEMENT_MESSAGE}`));
  }
  const d = renderPlaceholdersDetailed(text, {
    member: interaction.member, user: interaction.user, guild: interaction.guild, channel: interaction.channel, vars: sampleVars(interaction.member),
  }, { maxLength: 2000 });
  if (d.text.trim() === '') return send(c, fail('Empty Message', 'That text renders as an empty message.'));
  await updateSettings(guildId, { achievementMessage: text });
  const out = okEmbed(c, 'Announcement Message Updated',
    `Preview below.${settings.achievementAnnounceMode === 'OFF' ? '\nℹ️ Announcements are disabled; turn them on with /achievement-config announce mode.' : ''}`);
  out.embed.addFields({ name: 'Preview', value: clampText(d.text, 1024) });
  if (d.unknownTokens.length) {
    out.embed.addFields({ name: 'Unknown variables', value: joinWithLimit(d.unknownTokens.map((t) => `\`${t}\``), { max: 1024 }) });
  }
  return send(c, out);
}

const HANDLERS = Object.freeze({
  create,
  edit,
  delete: remove,
  enable: (c) => toggle(c, true),
  disable: (c) => toggle(c, false),
  list,
  info,
  reset,
  preset,
  'tier add': tierAdd,
  'tier edit': tierEdit,
  'tier remove': tierRemove,
  'announce mode': announceMode,
  'announce channel': announceChannel,
  'announce message': announceMessage,
});

export async function execute(interaction, client) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  try {
    // delete and reset prompt for confirmation, so their defer is ephemeral; the result is posted publicly afterwards.
    const ephemeral = key === 'delete' || key === 'reset';
    if (!(await safeDefer(interaction, { ephemeral }))) return;
    if (!(await resolveNewOptions(interaction, data, key === 'announce channel' ? { required: ['channel'] } : undefined))) return;
    const settings = await getSettings(interaction.guildId);
    const handler = HANDLERS[key];
    if (!handler) return;
    await handler({
      interaction, client, settings, guildId: interaction.guildId, moduleOff: !isModuleEnabled(settings, 'achievements'),
    });
  } catch (err) {
    logger.error(`achievement-config ${key} failed`, err);
    await safeReply(interaction, { embeds: [errorEmbed(...GENERIC_ERROR)], ephemeral: true });
  }
}

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    if (focused?.name !== 'achievement') {
      await respondAutocomplete(interaction, []);
      return;
    }
    const text = String(focused.value ?? '').trim().slice(0, 50);
    const budget = ackBudgetMs(interaction);
    let rows = [];
    if (budget > 0) {
      try {
        rows = await withTimeout(searchAchievements(interaction.guildId, text), budget, 'achievement autocomplete');
      } catch {
        rows = [];
      }
    }
    await respondAutocomplete(interaction, rows.map((a) => ({
      name: `${a.emoji ?? TRIGGER_EMOJI[a.trigger]} ${a.name} · ${TRIGGER_LABELS[a.trigger]}${a.enabled ? '' : ' (off)'}`,
      value: a.id,
    })));
  } catch (err) {
    logger.debug(`achievement-config autocomplete failed: ${err?.message}`);
  }
}
