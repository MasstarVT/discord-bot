import { ChannelType, PermissionFlagsBits, SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  getSettings,
  getSettingsSafe,
  peekSettings,
  updateSettings,
  updateSettingsWith,
} from '../../services/settingsService.js';
import { cache } from '../../services/redis.js';
import {
  ACTION_LABELS,
  DELETING_ACTIONS,
  EXEMPT_CAP,
  HONEYPOT,
  HONEYPOT_ACTION_LABELS,
  HONEYPOT_CAP,
  MAX_TIMEOUT_SECONDS,
  MIN_TIMEOUT_SECONDS,
  RULES,
  RULE_KEYS,
  describeHoneypot,
  describeHoneypotValue,
  describeLinksMode,
  describeRuleLine,
  getAutomodConfig,
  isLegacyConfig,
  normalizeConfig,
  normalizeDomain,
  normalizeWord,
  resolveRuleKey,
  stripInvisible,
} from '../../services/automodService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { applyGuildColor, errorEmbed, infoEmbed, neutralEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { checkChannelUsable, missingBotPermissions, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { formatDurationLong, parseDuration } from '../../utils/parseDuration.js';
import { paginate } from '../../utils/paginator.js';
import { compileSafeRegex, safeRegexTest, validateRegex } from '../../utils/safeRegex.js';
import { chunkLines, clampText, joinWithLimit } from '../../utils/text.js';

export const module = 'automod';
export const gated = false; // works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const ACTION_CHOICES = [
  { name: 'Off', value: 'off' },
  { name: 'Delete message', value: 'delete' },
  { name: 'Warn member', value: 'warn' },
  { name: 'Delete & warn', value: 'delete_warn' },
  { name: 'Delete & timeout', value: 'timeout' },
  { name: 'Delete & kick', value: 'kick' },
  { name: 'Delete & ban', value: 'ban' },
];

const EXEMPT_CHANNEL_TYPES = [
  ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildMedia,
  ChannelType.GuildVoice, ChannelType.GuildStageVoice, ChannelType.GuildCategory,
];

// A honeypot is a channel members can type in; never a category, forum or voice channel.
const HONEYPOT_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const HONEYPOT_CHOICES = [
  { name: 'Softban (default)', value: 'softban' },
  { name: 'Ban', value: 'ban' },
  { name: 'Kick', value: 'kick' },
  { name: 'Timeout', value: 'timeout' },
];
const HONEYPOT_CHANNEL_NAME = 'do-not-post';

export const data = new SlashCommandBuilder()
  .setName('automod')
  .setDescription('Auto-moderation rules, actions and exemptions (MEE6 Moderator › Auto-moderation).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Turn automod on. Your rules and lists stay as they are.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn automod off. Rules and lists are kept.'))
  .addSubcommand((s) => s.setName('status').setDescription('Show every rule with its action and scope, the lists and the exemptions.'))
  .addSubcommand((s) => s.setName('rule').setDescription("Set a rule's action, threshold and scope (Bad Words, Caps, Anti-Spam, …).")
    .addStringOption((o) => o.setName('rule').setDescription('The rule to configure.').setRequired(true).setAutocomplete(true))
    .addStringOption((o) => o.setName('action').setDescription('What happens when the rule matches.').setRequired(true).addChoices(...ACTION_CHOICES))
    .addIntegerOption((o) => o.setName('threshold').setDescription('Trigger level: caps %, emoji/spoiler/mention count, repeats, messages or marks.').setMinValue(1).setMaxValue(100))
    .addStringOption((o) => o.setName('timeout').setDescription('Timeout length for the timeout action, e.g. 10m, 2h, 1d (default 10m, max 28d).').setMaxLength(20))
    .addStringOption((o) => o.setName('roles').setDescription('Which members the rule applies to.')
      .addChoices({ name: 'Everyone except the listed roles', value: 'except' }, { name: 'Only the listed roles', value: 'only' }))
    .addStringOption((o) => o.setName('channels').setDescription('Which channels the rule applies in.')
      .addChoices({ name: 'Every channel except the listed ones', value: 'except' }, { name: 'Only the listed channels', value: 'only' })))
  .addSubcommandGroup((g) => g.setName('words').setDescription('Bad Words rule: words and phrases matched as whole words.')
    .addSubcommand((s) => s.setName('add').setDescription('Add words or phrases (comma-separated).')
      .addStringOption((o) => o.setName('words').setDescription('Words or phrases, separated by commas.').setRequired(true).setMaxLength(1000)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a word or phrase.')
      .addStringOption((o) => o.setName('word').setDescription('The word or phrase to remove.').setRequired(true).setAutocomplete(true).setMaxLength(64)))
    .addSubcommand((s) => s.setName('list').setDescription('List the filtered words (only you can see the list).')))
  .addSubcommandGroup((g) => g.setName('patterns').setDescription('Bad Words rule: regex patterns (checked for ReDoS before saving).')
    .addSubcommand((s) => s.setName('add').setDescription('Add a case-insensitive regex pattern.')
      .addStringOption((o) => o.setName('pattern').setDescription('Regex, e.g. fr[e3]{2}\\s*n[i1]tro').setRequired(true).setMaxLength(200)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a regex pattern.')
      .addStringOption((o) => o.setName('pattern').setDescription('The pattern to remove (pick from the list).').setRequired(true).setAutocomplete(true).setMaxLength(200)))
    .addSubcommand((s) => s.setName('list').setDescription('List the regex patterns (only you can see the list).')))
  .addSubcommandGroup((g) => g.setName('domains').setDescription('External Links rule: blocked and allowed domains.')
    .addSubcommand((s) => s.setName('add').setDescription('Add a domain to the blocked or allowed list.')
      .addStringOption((o) => o.setName('domain').setDescription('Domain or URL, e.g. example.com (subdomains included).').setRequired(true).setMaxLength(100))
      .addStringOption((o) => o.setName('list').setDescription('Which list (default: blocked).')
        .addChoices({ name: 'Blocked', value: 'blocked' }, { name: 'Allowed', value: 'allowed' })))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a domain from the blocked and allowed lists.')
      .addStringOption((o) => o.setName('domain').setDescription('The domain to remove.').setRequired(true).setAutocomplete(true).setMaxLength(100)))
    .addSubcommand((s) => s.setName('list').setDescription('Show the blocked and allowed domains.')))
  .addSubcommandGroup((g) => g.setName('exempt').setDescription("Exempt roles or channels from every rule, or edit one rule's lists.")
    .addSubcommand((s) => s.setName('add').setDescription("Add a role and/or channel to the exemptions or to a rule's list.")
      .addRoleOption((o) => o.setName('role').setDescription('Role to add.'))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category to add (threads follow their parent).').addChannelTypes(...EXEMPT_CHANNEL_TYPES))
      .addStringOption((o) => o.setName('rule').setDescription("Only this rule's list (leave empty for all rules).").setAutocomplete(true)))
    .addSubcommand((s) => s.setName('remove').setDescription("Remove a role and/or channel from the exemptions or a rule's list.")
      .addRoleOption((o) => o.setName('role').setDescription('Role to remove.'))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category to remove.').addChannelTypes(...EXEMPT_CHANNEL_TYPES))
      .addStringOption((o) => o.setName('rule').setDescription("Only this rule's list (leave empty for all rules).").setAutocomplete(true))))
  .addSubcommandGroup((g) => g.setName('honeypot').setDescription('Trap channels for spam bots: whoever posts in one is punished at once.')
    .addSubcommand((s) => s.setName('add').setDescription('Make a channel a honeypot, or create one.')
      .addChannelOption((o) => o.setName('channel').setDescription(`Leave empty to create #${HONEYPOT_CHANNEL_NAME} at the top.`)
        .addChannelTypes(...HONEYPOT_CHANNEL_TYPES))
      .addStringOption((o) => o.setName('action').setDescription('What happens to whoever posts there.').addChoices(...HONEYPOT_CHOICES))
      .addStringOption((o) => o.setName('timeout').setDescription('Timeout length, e.g. 1h or 1d (default 1d).').setMaxLength(20))
      .addBooleanOption((o) => o.setName('warning').setDescription('Post a warning in the channel (default: yes).')))
    .addSubcommand((s) => s.setName('remove').setDescription('Stop a channel being a honeypot (the channel is kept).')
      .addChannelOption((o) => o.setName('channel').setDescription('The honeypot.').setRequired(true).addChannelTypes(...HONEYPOT_CHANNEL_TYPES))));

// ── Helpers ───────────────────────────────────────────────────────────────────

const RULE_NAMES = RULE_KEYS.map((k) => RULES[k].label).join(', ').replace(/, ([^,]*)$/, ' or $1');
const UNKNOWN_RULE = () => errorEmbed('Unknown Rule', `Pick a rule from the list: ${RULE_NAMES}.`);
const SETTINGS_UNAVAILABLE = () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const SAVE_FAILED = () => errorEmbed('Could Not Save', 'The automod settings could not be saved right now. Please try again in a moment.');

/**
 * Read-modify-write of automodConfig under the settings row lock. `mutate(cfg, report)` is pure: it edits
 * the normalised copy, fills `report`, and returns false when there is nothing to write.
 */
async function writeConfig(guildId, mutate) {
  const report = {};
  const settings = await updateSettingsWith(guildId, (current) => {
    for (const k of Object.keys(report)) delete report[k]; // a re-run must not see stale notes
    const cfg = normalizeConfig(current.automodConfig);
    const changed = mutate(cfg, report);
    return changed === false ? null : { automodConfig: cfg };
  });
  return { settings, report };
}

const offHint = (key, config) => (config.rules[key].action === 'off'
  ? `\nThe **${RULES[key].label}** rule is off. Turn it on with \`/automod rule rule:${key} action:…\`.`
  : '');

const roleMentions = (ids, max = 300) => joinWithLimit(ids.map((id) => `<@&${id}>`), { max });
const channelMentions = (ids, max = 300) => joinWithLimit(ids.map((id) => `<#${id}>`), { max });

/** Packs whole items into pages of at most `max` characters, joined with `sep`. */
function packItems(items, sep, max) {
  const pages = [];
  let current = '';
  for (const item of items) {
    const piece = clampText(item, max);
    if (current === '') current = piece;
    else if (current.length + sep.length + piece.length <= max) current += sep + piece;
    else { pages.push(current); current = piece; }
  }
  if (current !== '') pages.push(current);
  return pages;
}

const NO_RULES_PROBLEM = 'No rules are active. Use `/automod rule` to turn one on.';

/**
 * What would stop automod from working as configured. Evaluated over the active rules (every rule, or just `only`).
 * @param {import('discord.js').Guild} guild
 * @param {object} settings
 * @param {object} config a normalised v2 config
 * @param {{ only?: string|null }} [opts]
 * @returns {string[]}
 */
export function collectProblems(guild, settings, config, { only = null } = {}) {
  const problems = [];
  const keys = (only ? [only] : RULE_KEYS).filter((k) => config.rules[k].action !== 'off');
  const actions = new Set(keys.map((k) => config.rules[k].action));
  const missing = (perm) => missingBotPermissions(guild, [perm]).length > 0;

  if ([...actions].some((a) => DELETING_ACTIONS.has(a)) && missing('ManageMessages')) {
    problems.push('I need **Manage Messages** to delete messages.');
  }
  if (actions.has('timeout') && missing('ModerateMembers')) problems.push('I need **Timeout Members** for the timeout action.');
  if (actions.has('kick') && missing('KickMembers')) problems.push('I need **Kick Members** for the kick action.');
  if (actions.has('ban') && missing('BanMembers')) problems.push('I need **Ban Members** for the ban action.');
  if ((keys.includes('spam') || keys.includes('repeatedText')) && !cache.available()) {
    problems.push('Redis is unavailable right now, so Anti-Spam and Repeated Text are skipped until it is back.');
  }
  if (keys.includes('badWords') && config.badWords.length === 0 && config.badWordPatterns.length === 0) {
    problems.push('**Bad Words** has no words or patterns yet — add some with `/automod words add`.');
  }
  for (const k of keys) {
    const r = config.rules[k];
    if (r.roleMode === 'only' && r.roleIds.length === 0) {
      problems.push(`**${RULES[k].label}** applies to nobody: its roles mode is "only listed" but the list is empty.`);
    }
    if (r.channelMode === 'only' && r.channelIds.length === 0) {
      problems.push(`**${RULES[k].label}** applies to nobody: its channels mode is "only listed" but the list is empty.`);
    }
  }
  if (only === null && settings?.automodEnabled && keys.length === 0 && config.honeypot.channelIds.length === 0) {
    problems.push(NO_RULES_PROBLEM);
  }
  if (only === null) problems.push(...collectHoneypotProblems(guild, config));
  if (only === null && !settings?.modLogChannelId) {
    problems.push('No mod-log channel — set one with `/modconfig logchannel` to see automod actions.');
  }
  return problems;
}

const HONEYPOT_PERMISSIONS = Object.freeze({
  softban: ['BanMembers', 'Ban Members'], ban: ['BanMembers', 'Ban Members'],
  kick: ['KickMembers', 'Kick Members'], timeout: ['ModerateMembers', 'Timeout Members'],
});

/**
 * What would stop the honeypot from working: the punishment permission, and per channel that it still exists and
 * that I can see it (and delete in it, for kick and timeout; a ban clears the message itself).
 * @returns {string[]}
 */
export function collectHoneypotProblems(guild, config) {
  const hp = config.honeypot;
  if (!hp.channelIds.length) return [];
  const problems = [];
  const [perm, permName] = HONEYPOT_PERMISSIONS[hp.action];
  if (missingBotPermissions(guild, [perm]).length > 0) problems.push(`I need **${permName}** for the honeypot's action.`);
  const needs = hp.action === 'kick' || hp.action === 'timeout' ? ['ViewChannel', 'ManageMessages'] : ['ViewChannel'];
  for (const id of hp.channelIds) {
    const channel = guild.channels?.cache?.get(id);
    if (!channel) {
      problems.push(`A honeypot channel (\`${id}\`) was deleted. It is forgotten the next time you use \`/automod honeypot\`.`);
      continue;
    }
    const usable = checkChannelUsable(channel, { needs });
    if (!usable.ok) problems.push(`Honeypot <#${id}>: ${usable.reason}`);
  }
  return problems;
}

const problemLines = (problems) => problems.map((p) => `⚠\u{FE0F} ${p}`).join('\n');

// ── enable / disable / status ─────────────────────────────────────────────────

async function enable(interaction) {
  const settings = await updateSettingsWith(interaction.guildId, (cur) => ({
    automodEnabled: true,
    automodConfig: normalizeConfig(cur.automodConfig),
  }));
  const { config } = getAutomodConfig(settings);
  const active = RULE_KEYS.filter((k) => config.rules[k].action !== 'off');
  const head = active.length
    ? `Active rules:\n${active.map((k) => describeRuleLine(k, config.rules[k], config)).join('\n')}`
    : 'No rules are active yet. Turn one on with `/automod rule`.';
  // The head already says when no rule is on, so that warning is not repeated under it.
  const problems = collectProblems(interaction.guild, settings, config).filter((p) => p !== NO_RULES_PROBLEM);
  const body = problems.length ? `${head}\n\n${problemLines(problems)}` : head;
  await interaction.editReply({ embeds: [successEmbed('Automod Enabled', clampText(body, 4096))] });
}

async function disable(interaction) {
  await updateSettings(interaction.guildId, { automodEnabled: false });
  await interaction.editReply({
    embeds: [successEmbed('Automod Disabled', 'Automod is off. Your rules, lists and exemptions are kept.')],
  });
}

async function status(interaction) {
  const settings = await getSettings(interaction.guildId);
  const { config } = getAutomodConfig(settings);

  const description = [
    settings.automodEnabled ? '**Status:** ✅ On' : '**Status:** ❌ Off — turn it on with `/automod enable`.',
    settings.modLogChannelId ? `**Mod log:** <#${settings.modLogChannelId}>` : '**Mod log:** not set — use `/modconfig logchannel`.',
    'Staff always bypass automod: the server owner, admins, moderators, members with Manage Messages and immune roles (`/modconfig immune`).',
  ].join('\n');
  const title = '🤖 Auto-Moderation';
  const footer = isLegacyConfig(settings.automodConfig)
    ? 'Stored in the old format; it is upgraded the next time you change a setting.' : null;

  // Discord caps an embed at 6000 characters in total: fields are added while the budget lasts.
  let budget = 5800 - title.length - description.length - (footer?.length ?? 0);
  const fields = [];
  const addField = (name, value) => {
    const room = Math.min(1024, budget - name.length);
    if (room < 40 || !value) return;
    const text = clampText(value, room);
    fields.push({ name, value: text });
    budget -= name.length + text.length;
  };

  chunkLines(RULE_KEYS.map((k) => describeRuleLine(k, config.rules[k], config)), 1024)
    .forEach((chunk, i) => addField(i === 0 ? 'Rules' : 'Rules (cont.)', chunk));

  addField(`${HONEYPOT.emoji} ${HONEYPOT.label}`, describeHoneypotValue(config));

  addField('Lists', `Bad words: ${config.badWords.length} · Patterns: ${config.badWordPatterns.length} · `
    + `Blocked domains: ${config.blockedDomains.length} · Allowed domains: ${config.allowedDomains.length}\n${describeLinksMode(config)}`);

  addField('Exempt from all rules',
    `Roles: ${config.exemptRoleIds.length ? roleMentions(config.exemptRoleIds, 480) : 'none'}\n`
    + `Channels: ${config.exemptChannelIds.length ? channelMentions(config.exemptChannelIds, 480) : 'none'}`);

  const scopeLines = [];
  for (const k of RULE_KEYS) {
    const r = config.rules[k];
    if (!r.roleIds.length && !r.channelIds.length && r.roleMode === 'except' && r.channelMode === 'except') continue;
    scopeLines.push(`${RULES[k].label}: roles ${r.roleMode} ${r.roleIds.length ? roleMentions(r.roleIds, 150) : 'none'}; `
      + `channels ${r.channelMode} ${r.channelIds.length ? channelMentions(r.channelIds, 150) : 'none'}`);
  }
  chunkLines(scopeLines, 1024).forEach((chunk, i) => addField(i === 0 ? 'Rule scopes' : 'Rule scopes (cont.)', chunk));

  const problems = collectProblems(interaction.guild, settings, config);
  if (problems.length) addField('⚠\u{FE0F} Problems', problems.join('\n'));

  const embed = applyGuildColor(neutralEmbed(title, description, footer ?? undefined), settings).addFields(fields);
  await interaction.editReply({ embeds: [embed] });
}

// ── rule ──────────────────────────────────────────────────────────────────────

async function ruleSet(interaction) {
  const key = resolveRuleKey(interaction.options.getString('rule', true));
  if (!key) return interaction.editReply({ embeds: [UNKNOWN_RULE()] });
  const action = interaction.options.getString('action', true);
  const threshold = interaction.options.getInteger('threshold');
  const timeout = interaction.options.getString('timeout');
  const roles = interaction.options.getString('roles');
  const channels = interaction.options.getString('channels');
  const meta = RULES[key];
  const invalid = (msg) => interaction.editReply({ embeds: [errorEmbed('Invalid Option', msg)] });

  if (threshold !== null && !meta.threshold) return invalid(`**${meta.label}** has no threshold.`);
  if (threshold !== null && (threshold < meta.threshold.min || threshold > meta.threshold.max)) {
    return invalid(`The threshold for **${meta.label}** must be between ${meta.threshold.min} and ${meta.threshold.max} (${meta.threshold.unit}).`);
  }
  if (timeout !== null && action !== 'timeout') {
    return invalid('The `timeout` option only applies to the **Delete & timeout** action.');
  }
  let ms = null;
  if (timeout !== null) {
    ms = parseDuration(timeout);
    if (ms === null || ms < MIN_TIMEOUT_SECONDS * 1000 || ms > MAX_TIMEOUT_SECONDS * 1000) {
      return invalid('Timeout must be between 1 minute and 28 days, e.g. `10m`, `2h` or `1d`.');
    }
  }

  const { settings } = await writeConfig(interaction.guildId, (cfg) => {
    Object.assign(cfg.rules[key], {
      action,
      ...(threshold !== null && { threshold }),
      ...(ms !== null && { timeoutSeconds: Math.floor(ms / 1000) }),
      ...(roles && { roleMode: roles }),
      ...(channels && { channelMode: channels }),
    });
    return true;
  });

  const { config } = getAutomodConfig(settings);
  const r = config.rules[key];
  const lines = [describeRuleLine(key, r, config)];
  lines.push(r.roleMode === 'only'
    ? `Roles: only ${r.roleIds.length ? roleMentions(r.roleIds) : 'none listed (applies to nobody)'}`
    : `Roles: everyone${r.roleIds.length ? ` except ${roleMentions(r.roleIds)}` : ''}`);
  lines.push(r.channelMode === 'only'
    ? `Channels: only ${r.channelIds.length ? channelMentions(r.channelIds) : 'none listed (applies to nobody)'}`
    : `Channels: every channel${r.channelIds.length ? ` except ${channelMentions(r.channelIds)}` : ''}`);
  if (r.action === 'off') lines.push(`**${meta.label}** is now off.`);
  const problems = collectProblems(interaction.guild, settings, config, { only: key });
  if (!settings.automodEnabled) problems.push('Automod is off — turn it on with `/automod enable`.');
  const body = problems.length ? `${lines.join('\n')}\n\n${problemLines(problems)}` : lines.join('\n');
  return interaction.editReply({ embeds: [successEmbed('Rule Updated', clampText(body, 4096))] });
}

// ── words (ephemeral) ─────────────────────────────────────────────────────────

/** A listed word as a spoiler. Markdown in it (the whole-word `_word_` form) would otherwise turn into italics. */
const spoilered = (w) => `||${escapeMarkdown(String(w))}||`;

async function wordsAdd(interaction) {
  const raw = interaction.options.getString('words', true);
  const valid = [];
  const invalid = [];
  for (const part of raw.split(/[,\n]/)) {
    const cleaned = stripInvisible(part.normalize('NFKC')).trim().toLowerCase().replace(/\s+/g, ' ');
    if (!cleaned) continue;
    const w = normalizeWord(cleaned);
    if (w !== null) valid.push(w);
    else if (cleaned.length < 2) invalid.push(`${escapeMarkdown(clampText(cleaned, 40))} (too short)`);
    else if (cleaned.length > 64) invalid.push(`${escapeMarkdown(clampText(cleaned, 40))} (too long)`);
    else invalid.push(`${escapeMarkdown(clampText(cleaned, 40))} (no letters or digits)`);
  }

  const { settings, report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    rep.added = [];
    rep.skipped = [];
    for (const w of valid) {
      if (cfg.badWords.includes(w)) rep.skipped.push(`${escapeMarkdown(w)} (already listed)`);
      else if (cfg.badWords.length >= LIMITS.AUTOMOD_WORDS) rep.skipped.push(`${escapeMarkdown(w)} (list full: ${LIMITS.AUTOMOD_WORDS})`);
      else { cfg.badWords.push(w); rep.added.push(w); }
    }
    return rep.added.length > 0;
  });

  const added = report.added ?? [];
  const notes = [...(report.skipped ?? []), ...invalid];
  if (added.length === 0) {
    const text = notes.length ? `Skipped: ${notes.join(', ')}` : 'No words were given.';
    return interaction.editReply({ embeds: [warnEmbed('Nothing Added', clampText(text, 4096))] });
  }
  const { config } = getAutomodConfig(settings);
  let body = clampText(`Added **${added.length}**: ${added.map(spoilered).join(', ')}`, 3500);
  if (notes.length) body += `\nSkipped: ${clampText(notes.join(', '), 400)}`;
  body += offHint('badWords', config);
  return interaction.editReply({ embeds: [successEmbed('Words Added', clampText(body, 4096))] });
}

async function wordsRemove(interaction) {
  const w = normalizeWord(interaction.options.getString('word', true));
  const { report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    const idx = w === null ? -1 : cfg.badWords.indexOf(w);
    if (idx < 0) { rep.notFound = true; return false; }
    cfg.badWords.splice(idx, 1);
    return true;
  });
  if (w === null || report.notFound) {
    return interaction.editReply({ embeds: [warnEmbed('Not Listed', 'That word is not on the list.')] });
  }
  return interaction.editReply({ embeds: [successEmbed('Word Removed', `${spoilered(w)} is no longer filtered.`)] });
}

async function wordsList(interaction) {
  const settings = await getSettings(interaction.guildId);
  const { config } = getAutomodConfig(settings);
  if (config.badWords.length === 0) {
    return interaction.editReply({ embeds: [infoEmbed('Bad Words', 'The list is empty. Add words with `/automod words add`.')] });
  }
  const pages = packItems(config.badWords.map(spoilered), ', ', 3900)
    .map((text) => neutralEmbed(`🤬 Bad Words (${config.badWords.length})`, text));
  if (pages.length === 1) return interaction.editReply({ embeds: pages });
  return paginate(interaction, pages, { ephemeral: true });
}

// ── patterns (ephemeral) ──────────────────────────────────────────────────────

async function patternsAdd(interaction) {
  const p = interaction.options.getString('pattern', true).trim();
  const v = validateRegex(p, 'i');
  if (!v.ok) return interaction.editReply({ embeds: [errorEmbed('Invalid Pattern', v.reason)] });
  const re = compileSafeRegex(p, 'i');
  if (!re) return interaction.editReply({ embeds: [errorEmbed('Invalid Pattern', 'This pattern could not be compiled.')] });
  if (safeRegexTest(re, '') || safeRegexTest(re, ' ')) {
    return interaction.editReply({
      embeds: [errorEmbed('Invalid Pattern', 'This pattern matches empty text, so it would flag every message.')],
    });
  }

  const { settings, report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    if (cfg.badWordPatterns.includes(p)) { rep.duplicate = true; return false; }
    if (cfg.badWordPatterns.length >= LIMITS.AUTOMOD_PATTERNS) { rep.full = true; return false; }
    cfg.badWordPatterns.push(p);
    rep.index = cfg.badWordPatterns.length;
    return true;
  });

  if (report.duplicate) return interaction.editReply({ embeds: [warnEmbed('Already Listed', 'That pattern is already on the list.')] });
  if (report.full) {
    return interaction.editReply({ embeds: [errorEmbed('List Full', `You can have at most ${LIMITS.AUTOMOD_PATTERNS} patterns.`)] });
  }
  const { config } = getAutomodConfig(settings);
  const body = `Pattern **#${report.index}** saved: \`${clampText(p, 200)}\`${offHint('badWords', config)}`;
  return interaction.editReply({ embeds: [successEmbed('Pattern Added', clampText(body, 4096))] });
}

async function patternsRemove(interaction) {
  const option = interaction.options.getString('pattern', true).trim();
  const { report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    const list = cfg.badWordPatterns;
    let idx = -1;
    if (/^\d{1,3}$/.test(option)) {
      const i = Number(option) - 1;
      if (i >= 0 && i < list.length) idx = i;
    }
    if (idx < 0) idx = list.indexOf(option);
    if (idx < 0) { rep.notFound = true; return false; }
    [rep.removed] = list.splice(idx, 1);
    return true;
  });
  if (report.notFound) {
    return interaction.editReply({ embeds: [warnEmbed('Not Listed', 'No pattern matches that. Pick one from the list.')] });
  }
  return interaction.editReply({
    embeds: [successEmbed('Pattern Removed', `\`${clampText(report.removed, 200)}\` removed.`)],
  });
}

async function patternsList(interaction) {
  const settings = await getSettings(interaction.guildId);
  const { config } = getAutomodConfig(settings);
  if (config.badWordPatterns.length === 0) {
    return interaction.editReply({ embeds: [infoEmbed('Regex Patterns', 'No patterns yet. Add one with `/automod patterns add`.')] });
  }
  const lines = config.badWordPatterns.map((p, i) => `**#${i + 1}** \`${p.replace(/`/g, 'ˋ')}\``);
  const pages = chunkLines(lines, 3900)
    .map((text) => neutralEmbed(`🧩 Regex Patterns (${config.badWordPatterns.length})`, text));
  if (pages.length === 1) return interaction.editReply({ embeds: pages });
  return paginate(interaction, pages, { ephemeral: true });
}

// ── domains ───────────────────────────────────────────────────────────────────

async function domainsAdd(interaction) {
  const d = normalizeDomain(interaction.options.getString('domain', true));
  if (d === null) {
    return interaction.editReply({
      embeds: [errorEmbed('Invalid Domain', "That doesn't look like a domain. Use something like `example.com`.")],
    });
  }
  const list = interaction.options.getString('list') ?? 'blocked';
  const other = list === 'blocked' ? 'allowed' : 'blocked';
  const field = (name) => (name === 'blocked' ? 'blockedDomains' : 'allowedDomains');

  const { settings, report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    const target = cfg[field(list)];
    const opposite = cfg[field(other)];
    if (target.includes(d)) { rep.duplicate = true; return false; }
    if (target.length >= LIMITS.AUTOMOD_DOMAINS) { rep.full = true; return false; }
    const at = opposite.indexOf(d);
    if (at >= 0) { opposite.splice(at, 1); rep.moved = true; }
    target.push(d);
    return true;
  });

  if (report.duplicate) {
    return interaction.editReply({ embeds: [warnEmbed('Already Listed', `**${d}** is already on the ${list} list.`)] });
  }
  if (report.full) {
    return interaction.editReply({ embeds: [errorEmbed('List Full', `Each domain list holds at most ${LIMITS.AUTOMOD_DOMAINS} domains.`)] });
  }
  const { config } = getAutomodConfig(settings);
  let body = `**${d}** (and its subdomains) is now **${list}**.`;
  if (report.moved) body += `\nIt was removed from the ${other} list.`;
  body += `\n${describeLinksMode(config)}${offHint('links', config)}`;
  return interaction.editReply({ embeds: [successEmbed('Domain Added', clampText(body, 4096))] });
}

async function domainsRemove(interaction) {
  const input = interaction.options.getString('domain', true);
  const d = normalizeDomain(input) ?? input.trim().toLowerCase();
  const { report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    rep.from = [];
    for (const [name, key] of [['blocked', 'blockedDomains'], ['allowed', 'allowedDomains']]) {
      const at = cfg[key].indexOf(d);
      if (at >= 0) { cfg[key].splice(at, 1); rep.from.push(name); }
    }
    return rep.from.length > 0;
  });
  if (!report.from?.length) {
    return interaction.editReply({ embeds: [warnEmbed('Not Listed', 'That domain is on neither list.')] });
  }
  return interaction.editReply({
    embeds: [successEmbed('Domain Removed', `**${d}** removed from the ${report.from.join(' and ')} list.`)],
  });
}

async function domainsList(interaction) {
  const settings = await getSettings(interaction.guildId);
  const { config } = getAutomodConfig(settings);
  const embed = applyGuildColor(neutralEmbed('🔗 External Links — Domains', describeLinksMode(config)), settings).addFields(
    { name: 'Blocked', value: config.blockedDomains.length ? joinWithLimit(config.blockedDomains, { max: 1024 }) : 'none' },
    { name: 'Allowed', value: config.allowedDomains.length ? joinWithLimit(config.allowedDomains, { max: 1024 }) : 'none' },
  );
  return interaction.editReply({ embeds: [embed] });
}

// ── exempt ────────────────────────────────────────────────────────────────────

async function exemptChange(interaction, mode) {
  const role = interaction.options.getRole('role');
  const channel = interaction.options.getChannel('channel');
  const ruleOpt = interaction.options.getString('rule');
  if (!role && !channel) {
    return interaction.editReply({ embeds: [errorEmbed('Nothing Selected', 'Pick a role, a channel, or both.')] });
  }
  if (role?.id === interaction.guildId) {
    return interaction.editReply({
      embeds: [errorEmbed('Invalid Role', "@everyone cannot be used here. Use /automod disable or a rule's action instead.")],
    });
  }
  let key = null;
  if (ruleOpt) {
    key = resolveRuleKey(ruleOpt);
    if (!key) return interaction.editReply({ embeds: [UNKNOWN_RULE()] });
  }

  const items = [];
  if (role) items.push({ kind: 'role', id: role.id, text: `<@&${role.id}>` });
  if (channel) items.push({ kind: 'channel', id: channel.id, text: `<#${channel.id}>` });

  const { settings, report } = await writeConfig(interaction.guildId, (cfg, rep) => {
    rep.lines = [];
    let changed = false;
    for (const item of items) {
      const list = item.kind === 'role'
        ? (key ? cfg.rules[key].roleIds : cfg.exemptRoleIds)
        : (key ? cfg.rules[key].channelIds : cfg.exemptChannelIds);
      const at = list.indexOf(item.id);
      if (mode === 'add') {
        if (at >= 0) rep.lines.push(`${item.text} was already listed`);
        else if (list.length >= EXEMPT_CAP) rep.lines.push(`${item.text} was not added: the list is full (${EXEMPT_CAP})`);
        else { list.push(item.id); rep.lines.push(`Added ${item.text}`); changed = true; }
      } else if (at < 0) {
        rep.lines.push(`${item.text} was not listed`);
      } else {
        list.splice(at, 1);
        rep.lines.push(`Removed ${item.text}`);
        changed = true;
      }
    }
    return changed;
  });

  const lines = [...(report.lines ?? [])];
  const changed = lines.some((l) => l.startsWith('Added ') || l.startsWith('Removed '));
  if (!changed) return interaction.editReply({ embeds: [warnEmbed('No Change', clampText(lines.join('\n'), 4096))] });

  const { config } = getAutomodConfig(settings);
  const roles = key ? config.rules[key].roleIds : config.exemptRoleIds;
  const channels = key ? config.rules[key].channelIds : config.exemptChannelIds;
  const rolesText = roles.length ? roleMentions(roles, 700) : 'none';
  const channelsText = channels.length ? channelMentions(channels, 700) : 'none';
  if (!key) {
    lines.push(`Exempt from every rule: roles: ${rolesText}; channels: ${channelsText}`);
  } else {
    const r = config.rules[key];
    if (r.roleMode === 'except' && r.channelMode === 'except') {
      lines.push(`**${RULES[key].label}** ignores: roles: ${rolesText}; channels: ${channelsText}`);
    } else if (r.roleMode === 'only' && r.channelMode === 'only') {
      lines.push(`**${RULES[key].label}** applies only to: roles: ${rolesText}; channels: ${channelsText}`);
    } else {
      lines.push(`**${RULES[key].label}** scope: roles ${r.roleMode} ${rolesText}; channels ${r.channelMode} ${channelsText}`);
    }
  }
  if (mode === 'add' && channel?.type === ChannelType.GuildCategory) {
    lines.push('Every channel and thread inside this category is covered.');
  }
  return interaction.editReply({ embeds: [successEmbed('Exemptions Updated', clampText(lines.join('\n'), 4096))] });
}

// ── honeypot ─────────────────────────────────────────────────────────────────

const HONEYPOT_OUTCOMES = Object.freeze({
  softban: 'removed from the server, and their messages from the last day are deleted',
  ban: 'banned, and their messages from the last day are deleted',
  kick: 'kicked',
  timeout: 'timed out',
});

/** The warning posted in a honeypot channel. */
export function honeypotWarningEmbed(action) {
  return warnEmbed(
    `${HONEYPOT.emoji} Do not post here`,
    'This channel is a trap for spam bots and hacked accounts.\n\n'
      + `**Anyone who posts here is automatically ${HONEYPOT_OUTCOMES[action] ?? HONEYPOT_OUTCOMES.softban}.**\n\n`
      + 'If you are a real person, just ignore this channel.',
  );
}

/** Drops honeypot channels that were deleted. */
const forgetDeleted = (cfg, guild) => {
  cfg.honeypot.channelIds = cfg.honeypot.channelIds.filter((id) => guild.channels.cache.has(id));
};

const HONEYPOT_FULL = () => errorEmbed('Too Many Honeypots',
  `You already have ${HONEYPOT_CAP} honeypot channels. Remove one with \`/automod honeypot remove\` first.`);

async function honeypotAdd(interaction) {
  const guild = interaction.guild;
  const action = interaction.options.getString('action');
  const timeout = interaction.options.getString('timeout');
  const postWarning = interaction.options.getBoolean('warning') ?? true;
  const invalid = (msg) => interaction.editReply({ embeds: [errorEmbed('Invalid Option', msg)] });

  if (timeout !== null && action !== 'timeout') {
    return invalid('The `timeout` option only applies with `action:` **Delete & timeout**.');
  }
  let ms = null;
  if (timeout !== null) {
    ms = parseDuration(timeout);
    if (ms === null || ms < MIN_TIMEOUT_SECONDS * 1000 || ms > MAX_TIMEOUT_SECONDS * 1000) {
      return invalid('Timeout must be between 1 minute and 28 days, e.g. `1h` or `1d`.');
    }
  }

  let channel = interaction.options.getChannel('channel');
  let created = false;
  if (!channel) {
    const { config } = getAutomodConfig(await getSettings(guild.id));
    if (config.honeypot.channelIds.filter((id) => guild.channels.cache.has(id)).length >= HONEYPOT_CAP) {
      return interaction.editReply({ embeds: [HONEYPOT_FULL()] });
    }
    if (missingBotPermissions(guild, ['ManageChannels']).length > 0) {
      return interaction.editReply({ embeds: [errorEmbed('Missing Permission',
        'I need **Manage Channels** to create the honeypot channel. Give me the permission, or pick an existing channel.')] });
    }
    try {
      channel = await guild.channels.create({
        name: HONEYPOT_CHANNEL_NAME,
        type: ChannelType.GuildText,
        position: 0,
        topic: 'Do not post here. This channel is a trap for spam bots and hacked accounts.',
        reason: clampText(`Honeypot channel created with /automod honeypot add by ${interaction.user.tag}`, 512),
      });
      created = true;
    } catch (err) {
      logger.warn(`Could not create a honeypot channel in ${guild.id}: ${err?.message ?? err}`);
      return interaction.editReply({ embeds: [errorEmbed('Could Not Create Channel',
        'Discord refused to create the channel. Check my **Manage Channels** permission, or pick an existing channel.')] });
    }
  }

  const { settings, report } = await writeConfig(guild.id, (cfg, rep) => {
    forgetDeleted(cfg, guild);
    const hp = cfg.honeypot;
    if (!hp.channelIds.includes(channel.id)) {
      if (hp.channelIds.length >= HONEYPOT_CAP) {
        rep.full = true;
        return false;
      }
      hp.channelIds.push(channel.id);
    }
    if (action) hp.action = action;
    if (ms !== null) hp.timeoutSeconds = Math.floor(ms / 1000);
    return true;
  });
  if (report.full) {
    if (created) await channel.delete('Honeypot limit reached').catch(() => {});
    return interaction.editReply({ embeds: [HONEYPOT_FULL()] });
  }

  const { config } = getAutomodConfig(settings);
  const hp = config.honeypot;
  const problems = [];
  let warned = false;
  if (postWarning) {
    const usable = checkChannelUsable(channel);
    if (!usable.ok) problems.push(`The warning was not posted: ${usable.reason}`);
    else {
      try {
        await channel.send({ embeds: [honeypotWarningEmbed(hp.action)], allowedMentions: { parse: [] } });
        warned = true;
      } catch (err) {
        problems.push('The warning could not be posted in the channel.');
        logger.warn(`Could not post the honeypot warning in ${guild.id}: ${err?.message ?? err}`);
      }
    }
  }
  problems.push(...collectHoneypotProblems(guild, config));
  if (!settings.automodEnabled) problems.push('Automod is off, so the honeypot does nothing yet. Turn it on with `/automod enable`.');

  const timeoutText = hp.action === 'timeout' ? ` for ${formatDurationLong(hp.timeoutSeconds * 1000)}` : '';
  const lines = [
    `<#${channel.id}> is now a honeypot.${created ? ' I created it at the top of the channel list.' : ''}${warned ? ' A warning is posted in it.' : ''}`,
    `Whoever posts there: **${HONEYPOT_ACTION_LABELS[hp.action]}**${timeoutText}.`,
    'The owner, staff (moderators, Manage Messages, immune roles) and exempt roles are never caught, so staff can still post there.',
    'Keep it near the top and visible to everyone: spam bots post in the first channels they can see.',
    '',
    describeHoneypot(config),
  ];
  const body = problems.length ? `${lines.join('\n')}\n\n${problemLines(problems)}` : lines.join('\n');
  return interaction.editReply({ embeds: [successEmbed('Honeypot Set', clampText(body, 4096))] });
}

async function honeypotRemove(interaction) {
  const guild = interaction.guild;
  const channel = interaction.options.getChannel('channel', true);
  const { settings, report } = await writeConfig(guild.id, (cfg, rep) => {
    const before = cfg.honeypot.channelIds.length;
    rep.was = cfg.honeypot.channelIds.includes(channel.id);
    forgetDeleted(cfg, guild);
    cfg.honeypot.channelIds = cfg.honeypot.channelIds.filter((id) => id !== channel.id);
    return cfg.honeypot.channelIds.length !== before;
  });
  const { config } = getAutomodConfig(settings);
  if (!report.was) {
    return interaction.editReply({ embeds: [infoEmbed('Not a Honeypot',
      `<#${channel.id}> is not a honeypot channel.\n\n${describeHoneypot(config)}`)] });
  }
  return interaction.editReply({ embeds: [successEmbed('Honeypot Removed', clampText(
    `<#${channel.id}> is no longer a honeypot. The channel and its warning are kept; delete them if you no longer need them.\n\n`
      + describeHoneypot(config), 4096))] });
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

const HANDLERS = Object.freeze({
  enable,
  disable,
  status,
  rule: ruleSet,
  'words add': wordsAdd,
  'words remove': wordsRemove,
  'words list': wordsList,
  'patterns add': patternsAdd,
  'patterns remove': patternsRemove,
  'patterns list': patternsList,
  'domains add': domainsAdd,
  'domains remove': domainsRemove,
  'domains list': domainsList,
  'exempt add': (i) => exemptChange(i, 'add'),
  'exempt remove': (i) => exemptChange(i, 'remove'),
  'honeypot add': honeypotAdd,
  'honeypot remove': honeypotRemove,
});

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  // Replies for words and patterns echo slurs and filter-evasion patterns: they are private.
  const ephemeral = group === 'words' || group === 'patterns';
  await interaction.deferReply(ephemeral ? { ephemeral: true } : undefined);

  try {
    const handler = HANDLERS[group ? `${group} ${sub}` : sub];
    if (!handler) {
      await interaction.editReply({ embeds: [errorEmbed('Unknown Command', 'That subcommand does not exist.')] });
      return;
    }
    await handler(interaction);
  } catch (err) {
    logger.error('automod command failed', err);
    const readOnly = sub === 'status' || sub === 'list';
    await interaction.editReply({ embeds: [readOnly ? SETTINGS_UNAVAILABLE() : SAVE_FAILED()] }).catch(() => {});
  }
}

// ── Autocomplete (never throws; always answers through respondAutocomplete) ────

/** Settings for an autocomplete answer: the memo first, then one lookup bounded by the 3 s budget. */
async function autocompleteSettings(interaction) {
  const memo = peekSettings(interaction.guildId);
  if (memo) return memo;
  return withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'automod autocomplete').catch(() => null);
}

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    const q = String(focused.value ?? '').toLowerCase();

    if (focused.name === 'rule') {
      const settings = peekSettings(interaction.guildId); // sync only, no I/O
      const config = settings ? getAutomodConfig(settings).config : null;
      const choices = RULE_KEYS
        .filter((k) => k.toLowerCase().includes(q) || RULES[k].label.toLowerCase().includes(q))
        .map((k) => ({
          name: `${RULES[k].emoji} ${RULES[k].label} (${k})${config ? ` — ${ACTION_LABELS[config.rules[k].action]}` : ''}`,
          value: k,
        }));
      return await respondAutocomplete(interaction, choices);
    }

    const settings = await autocompleteSettings(interaction);
    if (!settings) return await respondAutocomplete(interaction, []);
    const { config } = getAutomodConfig(settings);

    if (focused.name === 'word') {
      return await respondAutocomplete(interaction,
        config.badWords.filter((w) => w.includes(q)).slice(0, 25).map((w) => ({ name: w, value: w })));
    }
    if (focused.name === 'pattern') {
      const choices = [];
      config.badWordPatterns.forEach((p, i) => {
        if (choices.length < 25 && (p.toLowerCase().includes(q) || String(i + 1).startsWith(q))) {
          choices.push({ name: clampText(`#${i + 1} ${p}`, 100), value: String(i + 1) });
        }
      });
      return await respondAutocomplete(interaction, choices);
    }
    if (focused.name === 'domain') {
      const all = [
        ...config.blockedDomains.map((d) => ({ d, kind: 'blocked' })),
        ...config.allowedDomains.map((d) => ({ d, kind: 'allowed' })),
      ];
      return await respondAutocomplete(interaction,
        all.filter((x) => x.d.includes(q)).slice(0, 25).map((x) => ({ name: `${x.d} (${x.kind})`, value: x.d })));
    }
    return await respondAutocomplete(interaction, []);
  } catch (err) {
    logger.debug(`automod autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}
