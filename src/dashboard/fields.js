// The dashboard's settings registry: every GuildSettings value an admin may edit, as field descriptors
// (src/dashboard/schema.js) grouped per module the way the dashboard shows them, plus the reading, validation,
// cross-field and storage rules the slash commands apply to the same values.
// Pure: imports only pure modules. Rules whose originals live in files that open Redis at import (the services and
// commands) are restated here with a pointer to the original; test/dashboard.test.js checks that they still agree.
//
// Descriptors may carry server-only hooks (functions, never sent to the browser):
//   read(row)                 the stored value of this field in a GuildSettings row (default: row[field])
//   check(value)              null, a message or { path, message } (path relative to the field), after validateValue
//   normalize(value)          the value to store, after check
//   write(value, draft, row)  puts the value into `draft` (the columns to write); default: draft[field] = value
// A field whose name is not a column (the automod rules, the captcha verify button) is "virtual": its write() merges
// it into the JSON column named in SETTINGS_COLUMNS.
import { domainToASCII } from 'node:url';
import { COIN_MAX, LIMITS } from '../config/constants.js';
import { MODULES, MODULE_NAMES } from '../config/modules.js';
import { normalizeTemplate } from '../utils/customEmbed.js';
import { renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import { MAX_PATTERN_LENGTH, compileSafeRegex, safeRegexTest, validateRegex } from '../utils/safeRegex.js';
import {
  FieldError, collectStoredRefs, emptyRefs, isPlainObject, isSnowflake, readValue, storedRefKeys, validateValue,
} from './schema.js';

/** Most fields one PATCH may change (every field once is far below the body limit). */
export const MAX_SETTINGS_CHANGES = 200;

// ── Restated constants (originals named; test/dashboard.test.js compares them) ──────────────────────────────

/** Default texts used while a message column is null (welcomeService, xpService, birthdayService, achievementService, plugins/liverole). */
export const DEFAULT_TEXTS = Object.freeze({
  welcomeMessage: 'Hey {user}, welcome to **{server}**!',
  welcomeDmMessage: 'Have a great time here in **{server}**!',
  goodbyeMessage: '**{user.idname}** just left the server',
  levelUpMessage: 'GG {player}, you just advanced to level {level}!',
  birthdayMessage: 'Happy birthday {user}! 🎂',
  birthdayMessageAge: 'Happy birthday {user}! They are now {age}! 🎂',
  achievementMessage: '🏆 {player} unlocked **{achievement}**!',
  liveRoleAnnounceMessage: '🔴 {user} is now live on {platform}!\n{url}',
  modmailGreeting: 'Thanks for your message! The staff of **{server}** will answer you here soon.',
  boostMessage: '💎 {user} just boosted **{server}**! Thank you! We now have {boosts} boosts.',
  bumpReminderMessage: '📣 The server can be bumped again! Use `/bump` to keep us near the top of Disboard.',
  bumpThanksMessage: '📣 Thanks for bumping, {user}! The next bump is possible {next}.',
});

/** DEFAULT_WELCOME_EMBED / DEFAULT_GOODBYE_EMBED (src/services/welcomeService.js): sent while the column is null. */
export const DEFAULT_EMBEDS = Object.freeze({
  welcomeEmbed: Object.freeze({
    title: '👋 Welcome to {server}!',
    description: 'You are our **{server.member_count.ordinal}** member. Enjoy your stay!',
    thumbnail: '{user.avatar}',
    color: 0x57F287,
    timestamp: true,
  }),
  goodbyeEmbed: Object.freeze({
    description: DEFAULT_TEXTS.goodbyeMessage,
    thumbnail: '{user.avatar}',
    color: 0x99AAB5,
    timestamp: true,
  }),
});

/** defaultPanelTemplate() and DEFAULT_VERIFY_BUTTON (src/services/captchaService.js). */
export const DEFAULT_CAPTCHA_PANEL = Object.freeze({
  content: null,
  embed: Object.freeze({
    title: '🛡️ Verification required',
    description: 'Welcome to **{server}**!\n\nTo make sure you are human, press **Verify** below. Once you pass, '
      + 'you get access to the rest of the server.',
    footer: Object.freeze({ text: 'Only you can see your captcha.' }),
  }),
  buttons: Object.freeze([]),
});
export const DEFAULT_VERIFY_BUTTON = Object.freeze({ label: 'Verify', style: 'SUCCESS' });

/** DEFAULT_RANK_CARD_STYLE (src/utils/profileCard.js). */
export const DEFAULT_RANK_CARD_STYLE = Object.freeze({
  background: '#2B2D31', backgroundUrl: null, accentColor: '#5865F2', textColor: '#FFFFFF', overlayOpacity: 0.5,
});

/** XP_RANGE_MAX (xpService), RANGE_MAX and WORK_COOLDOWN_MAX_MINUTES (economyService),
  MAX_AUTOROLE_DELAY_SECONDS (welcomeService). */
export const NUMBER_LIMITS = Object.freeze({
  xpRangeMax: 1000,
  economyRangeMax: 1_000_000,
  workCooldownMaxMinutes: 10_080,
  autoroleDelayMaxSeconds: 604_800,
  captchaTimeoutMinMinutes: 5,
  captchaTimeoutMaxMinutes: 10_080,
  timeoutMaxSeconds: 2_419_200, // 28 days, Discord's timeout maximum
  timedActionMaxSeconds: 31_536_000, // 365 days
});

/** The automod rule registry (RULES, RULE_KEYS in src/services/automodService.js). */
export const AUTOMOD_RULES = Object.freeze({
  badWords: { label: 'Bad Words', threshold: null, description: 'Removes messages that contain a listed word, phrase or pattern.' },
  repeatedText: { label: 'Repeated Text', threshold: { min: 2, max: 20, default: 3, unit: 'identical messages within 30 s' },
    description: 'Catches the same message sent over and over.' },
  invites: { label: 'Server Invites', threshold: null, description: 'Catches invite links to other Discord servers.' },
  links: { label: 'External Links', threshold: null,
    description: 'Catches links that are blocked, or not allowed when no domain is blocked.' },
  caps: { label: 'Excessive Caps', threshold: { min: 50, max: 100, default: 70,
    unit: '% capitals, messages with at least 10 letters' },
    description: 'Catches messages that are mostly capital letters.' },
  emojis: { label: 'Excessive Emojis', threshold: { min: 1, max: 50, default: 10, unit: 'emojis' },
    description: 'Catches messages with too many emojis.' },
  spoilers: { label: 'Excessive Spoilers', threshold: { min: 1, max: 50, default: 5, unit: 'spoilers' },
    description: 'Catches messages with too many spoiler tags or spoiler attachments.' },
  mentions: { label: 'Excessive Mentions', threshold: { min: 2, max: 50, default: 5, unit: 'mentions' },
    description: 'Catches messages that ping too many members or roles.' },
  zalgo: { label: 'Zalgo', threshold: { min: 3, max: 20, default: 4, unit: 'stacked combining marks' },
    description: 'Catches glitch text with piles of combining marks.' },
  spam: { label: 'Anti-Spam', threshold: { min: 3, max: 30, default: 6, unit: 'messages within 5 s' },
    description: 'Catches members who send messages too fast.' },
});
export const AUTOMOD_RULE_KEYS = Object.freeze(Object.keys(AUTOMOD_RULES));

/** ACTIONS + ACTION_LABELS (automodService). */
export const AUTOMOD_ACTIONS = Object.freeze([
  { value: 'off', label: 'Off' },
  { value: 'delete', label: 'Delete message' },
  { value: 'warn', label: 'Warn member' },
  { value: 'delete_warn', label: 'Delete & warn' },
  { value: 'timeout', label: 'Delete & timeout' },
  { value: 'kick', label: 'Delete & kick' },
  { value: 'ban', label: 'Delete & ban' },
]);

/** DEFAULT_ALLOWED_DOMAINS, DEFAULT/MIN/MAX_TIMEOUT_SECONDS (automodService). */
export const AUTOMOD_DEFAULTS = Object.freeze({
  allowedDomains: Object.freeze(['tenor.com', 'giphy.com', 'discord.com', 'discordapp.com', 'discordapp.net']),
  timeoutSeconds: 600,
  minTimeoutSeconds: 60,
  maxTimeoutSeconds: 2_419_200,
});

/** HONEYPOT_ACTIONS + HONEYPOT_ACTION_LABELS, HONEYPOT_DEFAULT_TIMEOUT_SECONDS, HONEYPOT_CAP (automodService). */
export const AUTOMOD_HONEYPOT = Object.freeze({
  actions: Object.freeze([
    { value: 'softban', label: 'Softban (kick and clear their messages)' },
    { value: 'ban', label: 'Ban and clear their messages' },
    { value: 'kick', label: 'Delete & kick' },
    { value: 'timeout', label: 'Delete & timeout' },
  ]),
  timeoutSeconds: 86_400,
  maxChannels: LIMITS.AUTOMOD_HONEYPOTS,
});

/** LOG_EVENTS (src/services/loggingService.js): key -> label, in display order. */
export const LOG_EVENTS = Object.freeze({
  messageDelete: 'Message deleted', messageEdit: 'Message edited', messageBulkDelete: 'Messages bulk-deleted',
  invitePosted: 'Invite posted', memberRoles: 'Member roles changed', memberNickname: 'Nickname changed',
  memberTimeout: 'Member timed out / untimed', memberAvatar: 'Server avatar changed', userUpdate: 'User updated (name / avatar)',
  memberBan: 'Member banned', memberUnban: 'Member unbanned', memberJoin: 'Member joined', memberLeave: 'Member left',
  voiceJoin: 'Voice channel joined', voiceLeave: 'Voice channel left', voiceMove: 'Voice channel switched',
  voiceState: 'Voice mute / deafen / stream', channelCreate: 'Channel created', channelUpdate: 'Channel updated',
  channelDelete: 'Channel deleted', threadCreate: 'Thread created', threadUpdate: 'Thread updated', threadDelete: 'Thread deleted',
  roleCreate: 'Role created', roleUpdate: 'Role updated', roleDelete: 'Role deleted', serverUpdate: 'Server edited',
  emojiCreate: 'Emoji created', emojiUpdate: 'Emoji updated', emojiDelete: 'Emoji deleted', inviteCreate: 'Invite created',
  inviteDelete: 'Invite deleted / expired',
});
const LOG_EVENT_KEYS = Object.keys(LOG_EVENTS);

/** TIER_ACTIONS + TIER_ACTION_LABEL (src/services/moderationService.js). */
export const TIER_ACTIONS = Object.freeze([
  { value: 'TIMEOUT', label: 'Timeout' },
  { value: 'MUTE', label: 'Mute (mute role)' },
  { value: 'KICK', label: 'Kick' },
  { value: 'TEMPBAN', label: 'Temporary ban' },
  { value: 'BAN', label: 'Permanent ban' },
]);

// ── Small pure helpers ──────────────────────────────────────────────────────────────────────────────────────

const clone = (v) => (v === undefined ? undefined : structuredClone(v));
const round2 = (n) => Math.round(n * 100) / 100;
const cap = (s) => s.charAt(0).toUpperCase() + s.slice(1);
const nonBlank = (v) => typeof v === 'string' && v.trim() !== '';

// Channel kinds the commands offer (addChannelTypes).
const POSTABLE = Object.freeze(['text', 'announcement']);
const ANY_CHANNEL = Object.freeze(['text', 'announcement', 'voice', 'stage', 'forum', 'media', 'category']);

/** True when an EmbedSpec would show something (hasEmbedContent in src/services/welcomeService.js). */
export function embedHasContent(spec) {
  if (!isPlainObject(spec)) return false;
  return nonBlank(spec.title) || nonBlank(spec.description) || (Array.isArray(spec.fields) && spec.fields.length > 0)
    || nonBlank(spec.author?.name) || nonBlank(spec.image);
}

/** The EmbedSpec stored in a column (any stored shape), or null. */
const storedEmbed = (raw) => (isPlainObject(raw) ? normalizeTemplate({ content: null, embed: raw, buttons: [] }).embed : null);

/** Only the keys that carry a value, nested objects too (serializeEmbed in welcomeService). */
function serializeEmbed(spec) {
  const present = (v) => v !== null && v !== undefined && v !== '';
  const copy = (o) => Object.fromEntries(Object.entries(o ?? {}).filter(([, v]) => present(v)));
  const out = {};
  for (const key of ['title', 'description', 'url', 'color', 'author', 'footer', 'thumbnail', 'image', 'fields', 'timestamp']) {
    const v = spec?.[key];
    if (!present(v) || v === false) continue;
    if (key === 'author' || key === 'footer') {
      const nested = copy(v);
      if (Object.keys(nested).length) out[key] = nested;
    } else if (key === 'fields') {
      if (Array.isArray(v) && v.length) out.fields = v.map(copy);
    } else {
      out[key] = v;
    }
  }
  return out;
}

/** An embed to store: null when it shows nothing (the bot then uses its default, or no embed). */
const embedToStored = (spec) => (embedHasContent(spec) ? serializeEmbed(spec) : null);

// Invisible and zero-width characters (stripInvisible in automodService).
const INVISIBLE_RE = /[\u{AD}\u{34F}\u{61C}\u{180E}\u{200B}-\u{200F}\u{202A}-\u{202E}\u{2060}-\u{206F}\u{FEFF}]/gu;

/** normalizeWord (automodService): NFKC, invisible characters removed, lower-cased, 2-64 characters with a letter or digit. */
export function normalizeAutomodWord(input) {
  const s = String(input ?? '').normalize('NFKC').replace(INVISIBLE_RE, '').trim().toLowerCase().replace(/\s+/g, ' ');
  if (s.length < 2 || s.length > 64 || !/[\p{L}\p{N}]/u.test(s)) return null;
  return s;
}

/** normalizeDomain (automodService): the punycode host without www., or null. */
export function normalizeAutomodDomain(input) {
  let s = String(input ?? '').trim().toLowerCase();
  if (!s) return null;
  if (s.includes('://')) {
    try { s = new URL(s).hostname; } catch { return null; }
  }
  s = s.split(/[/?#]/)[0].replace(/:\d+$/, '').replace(/^\*\./, '').replace(/^www\./, '').replace(/\.$/, '');
  s = domainToASCII(s);
  return /^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:[a-z]{2,63}|xn--[a-z0-9-]{1,59})$/.test(s) && s.length <= 253 ? s : null;
}

/** The /automod patterns rule: a safe regex that matches neither an empty nor a blank message. */
function patternProblem(p) {
  const v = validateRegex(p, 'i');
  if (!v.ok) return v.reason ?? 'is not a valid pattern.';
  const re = compileSafeRegex(p, 'i');
  if (!re) return 'is not a safe pattern.';
  if (safeRegexTest(re, '') || safeRegexTest(re, ' ')) return 'matches every message.';
  return null;
}

// validateCurrencyEmoji / validateCurrencyName (src/services/economyService.js).
const CURRENCY_CUSTOM_EMOJI_RE = /^<a?:[A-Za-z0-9_]{2,32}:(\d{17,20})>$/;
const CURRENCY_UNICODE_EMOJI_RE = new RegExp('^(?:\\p{Extended_Pictographic}|\\p{Regional_Indicator})'
  + '(?:\\p{Extended_Pictographic}|\\p{Regional_Indicator}|\\p{Emoji_Modifier}|\\u{FE0F}|\\u{200D}|\\u{20E3})*$', 'u');
const currencyEmojiOk = (v) => CURRENCY_CUSTOM_EMOJI_RE.test(v)
  || ([...v].length >= 1 && [...v].length <= 10 && CURRENCY_UNICODE_EMOJI_RE.test(v));

// UTC aliases of normalizeTimeZone (src/commands/admin/settings.js), stored as 'UTC'.
const UTC_ALIASES = new Set(['utc', 'gmt', 'uct', 'zulu', 'universal', 'greenwich', 'etc/utc', 'etc/gmt', 'etc/uct',
  'etc/zulu', 'etc/universal', 'etc/greenwich', 'gmt0', 'gmt+0', 'gmt-0', 'etc/gmt0', 'etc/gmt+0', 'etc/gmt-0']);

// LEGACY_TZ_NAMES (src/commands/admin/settings.js): ICU's legacy names -> the current tzdata names.
const LEGACY_TZ_NAMES = Object.freeze({
  'Africa/Asmera': 'Africa/Asmara', 'America/Buenos_Aires': 'America/Argentina/Buenos_Aires',
  'America/Catamarca': 'America/Argentina/Catamarca', 'America/Cordoba': 'America/Argentina/Cordoba',
  'America/Jujuy': 'America/Argentina/Jujuy', 'America/Mendoza': 'America/Argentina/Mendoza',
  'America/Coral_Harbour': 'America/Atikokan', 'America/Godthab': 'America/Nuuk',
  'America/Indianapolis': 'America/Indiana/Indianapolis', 'America/Louisville': 'America/Kentucky/Louisville',
  'Asia/Calcutta': 'Asia/Kolkata', 'Asia/Katmandu': 'Asia/Kathmandu', 'Asia/Rangoon': 'Asia/Yangon',
  'Asia/Saigon': 'Asia/Ho_Chi_Minh', 'Atlantic/Faeroe': 'Atlantic/Faroe', 'Europe/Kiev': 'Europe/Kyiv',
  'Pacific/Enderbury': 'Pacific/Kanton', 'Pacific/Ponape': 'Pacific/Pohnpei', 'Pacific/Truk': 'Pacific/Chuuk',
});

const zoneValid = (zone) => {
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: zone });
    return true;
  } catch {
    return false;
  }
};

/** The name /settings timezone would store for a valid zone (normalizeTimeZone): canonical, current tzdata spelling. */
function canonicalZone(zone) {
  if (UTC_ALIASES.has(zone.toLowerCase())) return 'UTC';
  if (!zoneValid(zone)) return zone;
  const resolved = new Intl.DateTimeFormat('en-US', { timeZone: zone }).resolvedOptions().timeZone;
  if (UTC_ALIASES.has(resolved.toLowerCase())) return 'UTC';
  const modern = LEGACY_TZ_NAMES[resolved];
  return modern && zoneValid(modern) ? modern : resolved;
}

const SAMPLE_ACHIEVEMENT_VARS = Object.freeze({
  player: '<@1>', 'player.name': 'Member', achievement: 'Chatterbox', 'achievement.name': 'Chatterbox',
  'achievement.description': 'Send messages', tier: 'Bronze', 'tier.emoji': '🥉', goal: '100', progress: '100', rewards: '',
});

// ── Descriptor builders ─────────────────────────────────────────────────────────────────────────────────────

const bool = (field, def, label, description, extra = {}) => ({ field, type: 'boolean', label, description, default: def,
  ...extra });
const int = (field, def, min, max, label, description, extra = {}) => ({ field, type: 'int', label, description,
  default: def, min, max, ...extra });
const channel = (field, label, description, { kinds = POSTABLE, nullable = true } = {}) => ({
  field, type: 'channel', label, description, default: null, channelTypes: [...kinds], ...(nullable ? { nullable: true } : {}),
});

/** A message text column whose null means "the default text" (shown as that text). */
function messageField(field, label, description, { maxLength, placeholders = [], nullable = true, trim = true } = {}) {
  return {
    field, type: 'text', label, description, default: DEFAULT_TEXTS[field], maxLength, multiline: true,
    ...(nullable ? { nullable: true } : { minLength: 0 }),
    ...(placeholders.length ? { placeholders } : {}),
    // Null means the default text; the bots that trim also treat a blank text as the default.
    read: (row) => {
      const v = row?.[field];
      if (typeof v !== 'string' || (trim && v.trim() === '')) return DEFAULT_TEXTS[field];
      return v;
    },
    ...(trim ? { normalize: (v) => v.trim() } : {}),
  };
}

// ── General (/settings) ─────────────────────────────────────────────────────────────────────────────────────

/** The settings of the General page: timezone, embed colour, prefix and timezone roles. */
export const GENERAL_GROUPS = [
  {
    id: 'general',
    title: 'Server settings',
    description: 'Settings every module uses.',
    fields: [
      {
        field: 'timezone', type: 'timezone', label: 'Time zone', default: 'UTC',
        description: 'Used for reminders, birthdays and every other schedule. Changes apply from the next run of each schedule.',
        normalize: canonicalZone,
      },
      {
        field: 'embedColor', type: 'color', format: 'int', nullable: true, min: 1, default: null, label: 'Embed colour',
        description: "Colour of the bot's informational embeds. Empty uses the default blurple.",
      },
      {
        field: 'customPrefix', type: 'text', label: 'Custom command prefix', default: '!', maxLength: 10, minLength: 0,
        description: 'Members run custom commands as <prefix>name, e.g. !rules. Empty (or "off") turns prefix '
          + 'commands off; /tag use still works.',
        pattern: '[^\\s`<>@#:\\\\/\\p{C}]{0,5}',
        patternMessage: 'The prefix can be at most 5 characters, without spaces or any of ` < > @ # : \\ /.',
        normalize: (v) => (v.toLowerCase() === 'off' ? '' : v),
      },
    ],
  },
  {
    id: 'timezone-roles',
    title: 'Timezone roles',
    description: 'Roles such as @CST that give members a time zone in /timestamp (/settings timezone-role).',
    fields: [
      {
        field: 'timezoneRoles', type: 'objectList', label: 'Timezone roles', maxItems: LIMITS.TIMEZONE_ROLES, default: [],
        description: 'Members with one of these roles get its time zone in /timestamp, unless they saved their own with '
          + '/timezone set; with several, the highest role wins. Hand the roles out with a Unique reaction-role panel.',
        itemFields: [
          { field: 'roleId', type: 'role', label: 'Role', required: true, default: null },
          { field: 'zone', type: 'timezone', label: 'Time zone', default: 'UTC' },
        ],
        read: (row) => (Array.isArray(row?.timezoneRoles) ? row.timezoneRoles : []),
        check: (list) => {
          const seen = new Set();
          for (const [i, l] of list.entries()) {
            if (seen.has(l.roleId)) return { path: `${i}.roleId`, message: 'That role is already in the list.' };
            seen.add(l.roleId);
          }
          return null;
        },
        normalize: (list) => list.map((l) => ({ roleId: l.roleId, zone: canonicalZone(l.zone) })),
      },
    ],
  },
];

// ── Moderation ──────────────────────────────────────────────────────────────────────────────────────────────

const escalationField = {
  field: 'infractionEscalation', type: 'objectList', label: 'Warning escalation', maxItems: LIMITS.ESCALATION_TIERS, default: [],
  description: 'Punishes a member automatically once they collect a number of warnings (optionally within a time window).',
  itemFields: [
    int('warnCount', 3, 1, 100, 'Warnings', 'How many warnings trigger this step.'),
    int('withinSeconds', null, 60, NUMBER_LIMITS.timedActionMaxSeconds, 'Within (seconds)',
      'Only warnings from this many seconds count. Empty counts every warning.', { nullable: true, unit: 'seconds' }),
    { field: 'action', type: 'enum', label: 'Punishment', options: TIER_ACTIONS, default: 'TIMEOUT' },
    int('durationSeconds', 3600, 60, NUMBER_LIMITS.timedActionMaxSeconds, 'Duration (seconds)',
      'Required for Timeout (at most 28 days) and Temporary ban; optional for Mute; not used for Kick and Ban.',
      { nullable: true, unit: 'seconds' }),
  ],
  read: (row) => normalizeEscalation(row?.infractionEscalation),
  check: escalationProblem,
  write: (value, draft, row) => { draft.infractionEscalation = escalationToStored(value, row); },
};

const MODERATION_GROUPS = [
  {
    id: 'case-log',
    title: 'Moderation log & notices',
    fields: [
      channel('modLogChannelId', 'Moderation log channel',
        'Every case (warn, timeout, kick, ban, ...) is posted here, even while the Audit Logs module is off. Empty '
          + 'turns case logging off.'),
      bool('modDmTargets', true, 'DM members when punished',
        'Members get a direct message when they are warned, timed out, kicked or banned.'),
      {
        field: 'modVisibility', type: 'enum', label: 'Command visibility', default: 'PUBLIC',
        description: 'Who sees the confirmation of a moderation command.',
        options: [
          { value: 'PUBLIC', label: 'Public' },
          { value: 'ANONYMOUS', label: 'Anonymous (moderator hidden)' },
          { value: 'PRIVATE', label: 'Private (only the moderator)' },
        ],
      },
    ],
  },
  {
    id: 'roles',
    title: 'Moderator roles',
    fields: [
      { field: 'modRoleIds', type: 'roles', label: 'Moderator roles', maxItems: LIMITS.MOD_ROLES, default: [],
        description: "Members with one of these roles can use the bot's moderator commands." },
      { field: 'immuneRoleIds', type: 'roles', label: 'Immune roles', maxItems: LIMITS.IMMUNE_ROLES, default: [],
        description: 'Members with one of these roles are never punished by the bot or by Auto-Moderation.' },
      { field: 'muteRoleId', type: 'role', label: 'Mute role', nullable: true, assignable: true, default: null,
        description: 'Used for mutes longer than 28 days (Discord timeouts cover the rest). /modconfig muterole '
          + 'setup:true creates one and locks it out of every channel.' },
    ],
  },
  { id: 'escalation', title: 'Automatic punishments', fields: [escalationField] },
];

// ── Auto-moderation (virtual fields over the automodConfig column) ─────────────────────────────────────────

const AUTOMOD_CHANNEL_KINDS = Object.freeze(['text', 'announcement', 'forum', 'media', 'voice', 'stage', 'category']);
const SCOPE_OPTIONS = (noun) => [
  { value: 'except', label: `Every ${noun} except the listed ones` },
  { value: 'only', label: `Only the listed ${noun}s` },
];

function defaultRule(key) {
  const t = AUTOMOD_RULES[key].threshold;
  return {
    action: 'off', ...(t ? { threshold: t.default } : {}), timeoutSeconds: AUTOMOD_DEFAULTS.timeoutSeconds,
    roleMode: 'except', roleIds: [], channelMode: 'except', channelIds: [],
  };
}

/** The rule descriptors, built once: rule key -> descriptor (field automodRule<Key>). */
const RULE_FIELDS = {};

/** The automod list fields: field -> key inside automodConfig. */
const AUTOMOD_LIST_KEYS = Object.freeze({
  automodBadWords: 'badWords',
  automodBadWordPatterns: 'badWordPatterns',
  automodBlockedDomains: 'blockedDomains',
  automodAllowedDomains: 'allowedDomains',
  automodExemptRoleIds: 'exemptRoleIds',
  automodExemptChannelIds: 'exemptChannelIds',
});

/** A full v2 automodConfig (normalizeConfig in automodService upgrades a v1 row first; see settingsBridge.js). */
export function automodConfigOf(row) {
  const raw = row?.automodConfig;
  const src = isPlainObject(raw) && raw.version === 2 ? raw : {};
  const rules = {};
  for (const key of AUTOMOD_RULE_KEYS) rules[key] = readValue(RULE_FIELDS[key],
    isPlainObject(src.rules) ? src.rules[key] : undefined);
  const cfg = { version: 2, rules };
  for (const [field, key] of Object.entries(AUTOMOD_LIST_KEYS)) {
    const desc = AUTOMOD_LIST_FIELDS[field];
    cfg[key] = desc.type === 'stringList' ? readList(desc, src[key]) : readValue(desc, src[key]);
  }
  // A missing allowedDomains key means the defaults; an explicit [] stays empty.
  if (!Array.isArray(src.allowedDomains)) cfg.allowedDomains = [...AUTOMOD_DEFAULTS.allowedDomains];
  cfg.honeypot = readValue(HONEYPOT_FIELD, src.honeypot);
  return cfg;
}

/** The list entries that are valid on their own (one bad entry never hides the rest). */
function readList(desc, raw) {
  if (!Array.isArray(raw)) return clone(desc.default);
  const out = [];
  for (const item of raw) {
    try {
      const [v] = validateValue({ ...desc, maxItems: 1 }, [item], desc.field);
      const n = desc.normalize ? desc.normalize([v])[0] : v;
      if (typeof n === 'string' && !out.includes(n)) out.push(n);
    } catch { /* an invalid entry is left out */ }
    if (out.length >= desc.maxItems) break;
  }
  return out;
}

function automodDraft(draft, row) {
  draft.automodConfig ??= automodConfigOf(row);
  return draft.automodConfig;
}

// The dashboard hides a rule's options while the rule is off (forms.js `showWhen`).
const RULE_ON = Object.freeze({ field: 'action', notIn: ['off'] });

for (const key of AUTOMOD_RULE_KEYS) {
  const meta = AUTOMOD_RULES[key];
  const t = meta.threshold;
  RULE_FIELDS[key] = {
    field: `automodRule${cap(key)}`,
    type: 'object',
    label: meta.label,
    description: meta.description,
    default: defaultRule(key),
    fields: [
      { field: 'action', type: 'enum', label: 'Action', options: AUTOMOD_ACTIONS, default: 'off' },
      ...(t ? [int('threshold', t.default, t.min, t.max, 'Threshold', `Triggers at ${t.min} to ${t.max} ${t.unit}.`,
        { unit: t.unit, showWhen: RULE_ON })] : []),
      int('timeoutSeconds', AUTOMOD_DEFAULTS.timeoutSeconds, AUTOMOD_DEFAULTS.minTimeoutSeconds, AUTOMOD_DEFAULTS.maxTimeoutSeconds,
        'Timeout length (seconds)', 'How long "Delete & timeout" times the member out (1 minute to 28 days).',
        { unit: 'seconds', showWhen: { field: 'action', in: ['timeout'] } }),
      { field: 'roleMode', type: 'enum', label: 'Applies to roles', options: SCOPE_OPTIONS('role'), default: 'except', showWhen: RULE_ON },
      { field: 'roleIds', type: 'roles', label: 'Roles', maxItems: LIMITS.AUTOMOD_EXEMPTIONS, default: [], showWhen: RULE_ON },
      { field: 'channelMode', type: 'enum', label: 'Applies in channels', options: SCOPE_OPTIONS('channel'), default: 'except',
        showWhen: RULE_ON },
      { field: 'channelIds', type: 'channels', label: 'Channels', channelTypes: [...AUTOMOD_CHANNEL_KINDS],
        maxItems: LIMITS.AUTOMOD_EXEMPTIONS, default: [], showWhen: RULE_ON },
    ],
    read: (row) => automodConfigOf(row).rules[key],
    write: (value, draft, row) => { automodDraft(draft, row).rules[key] = value; },
  };
}

// The honeypot (automodConfig.honeypot): text or announcement channels only, never a category (it would catch everyone).
const HONEYPOT_FIELD = {
  field: 'automodHoneypot',
  type: 'object',
  label: 'Honeypot channels',
  description: 'Trap channels nobody should type in. Spam bots and hacked accounts post everywhere, so whoever posts in '
    + 'one is punished at once. Staff and exempt roles are never caught. /automod honeypot add can create the channel '
    + 'and post a warning in it.',
  default: { channelIds: [], action: 'softban', timeoutSeconds: AUTOMOD_HONEYPOT.timeoutSeconds },
  fields: [
    { field: 'channelIds', type: 'channels', label: 'Channels', channelTypes: [...POSTABLE],
      maxItems: AUTOMOD_HONEYPOT.maxChannels, default: [] },
    { field: 'action', type: 'enum', label: 'Action', options: AUTOMOD_HONEYPOT.actions, default: 'softban',
      description: 'Softban and ban also delete their messages from the last day in every channel.' },
    int('timeoutSeconds', AUTOMOD_HONEYPOT.timeoutSeconds, AUTOMOD_DEFAULTS.minTimeoutSeconds, AUTOMOD_DEFAULTS.maxTimeoutSeconds,
      'Timeout length (seconds)', 'How long "Delete & timeout" times the member out (1 minute to 28 days).',
      { unit: 'seconds', showWhen: { field: 'action', in: ['timeout'] } }),
  ],
  read: (row) => automodConfigOf(row).honeypot,
  write: (value, draft, row) => { automodDraft(draft, row).honeypot = value; },
};

const wordsProblem = (list) => {
  const i = list.findIndex((w) => normalizeAutomodWord(w) === null);
  return i === -1 ? null : { path: String(i),
    message: `"${list[i].slice(0, 40)}" needs 2 to 64 characters with at least one letter or digit.` };
};
const patternsProblem = (list) => {
  for (const [i, p] of list.entries()) {
    const problem = patternProblem(p);
    if (problem) return { path: String(i), message: `"${p.slice(0, 40)}" ${problem}` };
  }
  return null;
};
const domainsProblem = (list) => {
  const i = list.findIndex((d) => normalizeAutomodDomain(d) === null);
  return i === -1 ? null : { path: String(i), message: `"${list[i].slice(0, 40)}" is not a domain such as example.com.` };
};
const dedupe = (list) => [...new Set(list)];

/** The automod list descriptors (virtual fields over automodConfig). */
const AUTOMOD_LIST_FIELDS = {
  automodBadWords: {
    field: 'automodBadWords', type: 'stringList', label: 'Bad words and phrases', default: [],
    description: 'Whole words or phrases, matched case-insensitively (also through spacing and markdown tricks).',
    maxItems: LIMITS.AUTOMOD_WORDS, itemMaxLength: 64, itemMinLength: 2, lowercase: true,
    check: wordsProblem, normalize: (list) => dedupe(list.map(normalizeAutomodWord)),
  },
  automodBadWordPatterns: {
    field: 'automodBadWordPatterns', type: 'stringList', label: 'Bad word patterns', default: [],
    description: 'Regular expressions (case-insensitive) for words the list cannot catch. Unsafe or catch-all patterns are refused.',
    maxItems: LIMITS.AUTOMOD_PATTERNS, itemMaxLength: MAX_PATTERN_LENGTH,
    commaSeparated: false, // a comma is part of a pattern (\d{2,4})
    check: patternsProblem,
  },
  automodBlockedDomains: {
    field: 'automodBlockedDomains', type: 'stringList', label: 'Blocked domains', default: [],
    description: 'Links to these sites are caught. While this list is empty, every link outside the allowed domains is caught.',
    maxItems: LIMITS.AUTOMOD_DOMAINS, itemMaxLength: 512,
    check: domainsProblem, normalize: (list) => dedupe(list.map(normalizeAutomodDomain)),
  },
  automodAllowedDomains: {
    field: 'automodAllowedDomains', type: 'stringList', label: 'Allowed domains', default: [...AUTOMOD_DEFAULTS.allowedDomains],
    description: 'Links to these sites are always fine. Adding a domain to one list takes it off the other.',
    maxItems: LIMITS.AUTOMOD_DOMAINS, itemMaxLength: 512,
    check: domainsProblem, normalize: (list) => dedupe(list.map(normalizeAutomodDomain)),
  },
  automodExemptRoleIds: {
    field: 'automodExemptRoleIds', type: 'roles', label: 'Exempt roles', default: [],
    description: 'Members with one of these roles are never checked by any rule.', maxItems: LIMITS.AUTOMOD_EXEMPTIONS,
  },
  automodExemptChannelIds: {
    field: 'automodExemptChannelIds', type: 'channels', label: 'Exempt channels', default: [],
    description: 'Messages in these channels (or categories) are never checked.',
    channelTypes: [...AUTOMOD_CHANNEL_KINDS], maxItems: LIMITS.AUTOMOD_EXEMPTIONS,
  },
};

const OTHER_DOMAIN_LIST = Object.freeze({ blockedDomains: 'allowedDomains', allowedDomains: 'blockedDomains' });

for (const [field, key] of Object.entries(AUTOMOD_LIST_KEYS)) {
  const desc = AUTOMOD_LIST_FIELDS[field];
  desc.read = (row) => automodConfigOf(row)[key];
  desc.write = (value, draft, row) => {
    const cfg = automodDraft(draft, row);
    cfg[key] = value;
    // Adding a domain to one list takes it off the other (as /automod domains does), unless both lists are saved together.
    const other = OTHER_DOMAIN_LIST[key];
    draft[TOUCHED] ??= new Set();
    if (other && !draft[TOUCHED].has(other)) cfg[other] = cfg[other].filter((d) => !value.includes(d));
    draft[TOUCHED].add(key);
  };
}

// Draft bookkeeping that is never written.
const TOUCHED = Symbol('automod lists written in this request');

const rule = (key) => RULE_FIELDS[key];
const automodList = (field) => AUTOMOD_LIST_FIELDS[field];

const AUTOMOD_GROUPS = [
  {
    id: 'bad-words',
    title: 'Bad words',
    description: 'Removes or punishes messages with words you do not want in the server.',
    fields: [rule('badWords'), automodList('automodBadWords'), automodList('automodBadWordPatterns')],
  },
  {
    id: 'links',
    title: 'Invites & links',
    fields: [rule('invites'), rule('links'), automodList('automodBlockedDomains'), automodList('automodAllowedDomains')],
  },
  {
    id: 'spam',
    title: 'Spam',
    fields: [rule('spam'), rule('repeatedText')],
  },
  {
    id: 'content',
    title: 'Message content',
    fields: [rule('caps'), rule('emojis'), rule('spoilers'), rule('mentions'), rule('zalgo')],
  },
  {
    id: 'honeypot',
    title: 'Honeypot',
    description: 'Works even while every rule is off, and an exempt channel does not shield it.',
    fields: [HONEYPOT_FIELD],
  },
  {
    id: 'exemptions',
    title: 'Exemptions',
    description: 'Staff (moderators, Manage Messages and immune roles) are always exempt.',
    fields: [automodList('automodExemptRoleIds'), automodList('automodExemptChannelIds')],
  },
];

// ── Audit logs ──────────────────────────────────────────────────────────────────────────────────────────────

const LOGGING_GROUPS = [
  {
    id: 'channels',
    title: 'Log channels',
    description: 'Each kind of event goes to its own channel. Empty turns that kind off.',
    fields: [
      channel('messageLogChannelId', 'Message log', 'Deleted, edited and bulk-deleted messages, and posted invites.'),
      channel('memberLogChannelId', 'Member log', 'Role, nickname, avatar and timeout changes, bans and unbans.'),
      channel('serverLogChannelId', 'Server log', 'Channels, threads, roles, emojis, invites and server edits.'),
      channel('voiceLogChannelId', 'Voice log', 'Members joining, leaving and switching voice channels.'),
      channel('joinLeaveChannelId', 'Joins & leaves log', 'Members joining and leaving the server.'),
    ],
  },
  {
    id: 'events',
    title: 'Logged events',
    fields: [
      // Checked = logged, but the column keeps the switched-off list (as /logging writes it), so an event added later
      // starts logged.
      {
        field: 'logEvents', type: 'multiEnum', label: 'Logged events', default: [...LOG_EVENT_KEYS],
        description: 'Checked events are logged. Unchecked ones are not.',
        options: LOG_EVENT_KEYS.map((value) => ({ value, label: LOG_EVENTS[value] })),
        read: (row) => {
          const off = Array.isArray(row?.disabledLogEvents) ? row.disabledLogEvents : [];
          return LOG_EVENT_KEYS.filter((k) => !off.includes(k));
        },
        normalize: (list) => LOG_EVENT_KEYS.filter((k) => list.includes(k)),
        write: (value, draft) => { draft.disabledLogEvents = LOG_EVENT_KEYS.filter((k) => !value.includes(k)); },
      },
    ],
  },
  {
    id: 'ignored',
    title: 'Ignored channels',
    fields: [
      { field: 'logIgnoredChannelIds', type: 'channels', label: 'Ignored channels', channelTypes: [...ANY_CHANNEL],
        maxItems: LIMITS.LOG_IGNORED_CHANNELS, default: [],
        description: 'Message events in these channels are never logged. A category covers its channels and threads.' },
    ],
  },
];

// ── Welcome & Goodbye ───────────────────────────────────────────────────────────────────────────────────────

/** welcomeCardEnabled + parseCardStyle (welcomeService): the style's own flag, then a legacy welcomeEmbed.card, else on. */
function welcomeCardStyleOf(row) {
  const raw = isPlainObject(row?.welcomeCardStyle) ? row.welcomeCardStyle : {};
  const color = (v) => (typeof v === 'string' && /^#[0-9a-f]{6}$/i.test(v) ? v.toUpperCase() : null);
  let enabled = typeof raw.enabled === 'boolean' ? raw.enabled : null;
  if (enabled === null) enabled = isPlainObject(row?.welcomeEmbed) ? row.welcomeEmbed.card !== false : true;
  const overlay = typeof raw.overlayOpacity === 'number' && Number.isFinite(raw.overlayOpacity)
    ? round2(Math.min(1, Math.max(0, raw.overlayOpacity))) : null;
  return {
    enabled,
    background: color(raw.background),
    backgroundUrl: typeof raw.backgroundUrl === 'string' && raw.backgroundUrl.length <= 2048 ? raw.backgroundUrl : null,
    textColor: color(raw.textColor),
    accentColor: color(raw.accentColor),
    overlayOpacity: overlay,
  };
}

const welcomeEmbedField = (field, label, description) => ({
  field, type: 'embed', label, description, nullable: true, default: clone(DEFAULT_EMBEDS[field]),
  read: (row) => {
    const spec = storedEmbed(row?.[field]);
    return embedHasContent(spec) ? spec : clone(DEFAULT_EMBEDS[field]);
  },
  normalize: embedToStored,
});

const WELCOME_GROUPS = [
  {
    id: 'welcome-message',
    title: 'Welcome message',
    description: 'Posted in a channel when someone joins.',
    fields: [
      bool('welcomeMessageEnabled', true, 'Welcome message in a channel',
        'Posts the welcome message in the welcome channel when someone joins.'),
      channel('welcomeChannelId', 'Welcome channel', 'Where the welcome message and card are posted.', { nullable: false }),
      bool('welcomeUseEmbed', true, 'Welcome message as an embed', 'Sends the channel welcome as an embed; off sends plain text.'),
      {
        ...messageField('welcomeMessage', 'Message text', 'The text line (pings work here). Leave it empty to send only the embed.',
          { maxLength: 2000, nullable: false, trim: false }),
        read: (row) => {
          const m = row?.welcomeMessage;
          if (typeof m === 'string') return m;
          // Null: the bot's default text, except next to a custom embed (legacy rows keep sending no text line there).
          return row?.welcomeUseEmbed !== false && embedHasContent(storedEmbed(row?.welcomeEmbed)) ? '' : DEFAULT_TEXTS.welcomeMessage;
        },
      },
      { ...welcomeEmbedField('welcomeEmbed', 'Embed',
        'Shown when "Welcome message as an embed" is on. Clear it to use the default embed.'),
      showWhen: { field: 'welcomeUseEmbed', in: [true] } },
    ],
  },
  {
    id: 'welcome-card',
    title: 'Welcome card',
    description: 'An image with the member\'s avatar and name, attached to the welcome message.',
    fields: [
      {
        field: 'welcomeCardStyle', type: 'object', label: 'Welcome card', default: welcomeCardStyleOf(null),
        description: 'Whether the card is sent, and its colours and background.',
        fields: [
          bool('enabled', true, 'Send a welcome card', 'Attaches the welcome card image to the welcome message.'),
          { field: 'background', type: 'color', format: 'hex', nullable: true, default: null, label: 'Background colour',
            description: 'Empty uses the default background.' },
          { field: 'backgroundUrl', type: 'url', nullable: true, maxLength: 2048, default: null, label: 'Background image',
            description: 'An http(s) link to a PNG, JPEG or WebP image (at most 5 MB). The bot downloads it once to check it.' },
          { field: 'textColor', type: 'color', format: 'hex', nullable: true, default: null, label: 'Text colour' },
          { field: 'accentColor', type: 'color', format: 'hex', nullable: true, default: null, label: 'Accent colour' },
          { field: 'overlayOpacity', type: 'float', min: 0, max: 1, step: 0.01, nullable: true, default: null,
            label: 'Overlay opacity',
            description: 'How much the background image is darkened (0 to 1). Empty uses the default.' },
        ],
        read: welcomeCardStyleOf,
        write: (value, draft) => {
          const lower = (c) => (typeof c === 'string' ? c.toLowerCase() : null);
          draft.welcomeCardStyle = {
            v: 1, enabled: value.enabled, background: lower(value.background), backgroundUrl: value.backgroundUrl ?? null,
            textColor: lower(value.textColor), accentColor: lower(value.accentColor),
            overlayOpacity: value.overlayOpacity === null ? null : round2(value.overlayOpacity),
          };
        },
      },
    ],
  },
  {
    id: 'welcome-dm',
    title: 'Welcome DM',
    description: 'A private message to every new member.',
    fields: [
      bool('welcomeDmEnabled', false, 'Welcome DM', 'Also sends new members a private welcome message.'),
      messageField('welcomeDmMessage', 'DM text', 'Leave it empty to send only the embed.', { maxLength: 2000,
        nullable: false, trim: false }),
      {
        field: 'welcomeDmEmbed', type: 'embed', label: 'DM embed', nullable: true, default: null,
        description: 'Optional. Clear it to send the text only.',
        read: (row) => { const spec = storedEmbed(row?.welcomeDmEmbed); return embedHasContent(spec) ? spec : null; },
        normalize: embedToStored,
      },
      bool('welcomeDmCard', false, 'Welcome card in the DM', 'Attaches the welcome card image to the welcome DM.'),
    ],
  },
  {
    id: 'goodbye',
    title: 'Goodbye message',
    description: 'Posted in a channel when a member leaves.',
    fields: [
      bool('goodbyeEnabled', false, 'Goodbye message', 'Posts a message in the goodbye channel when a member leaves.'),
      channel('goodbyeChannelId', 'Goodbye channel', 'Where the goodbye message is posted.', { nullable: false }),
      bool('goodbyeUseEmbed', false, 'Goodbye message as an embed', 'Sends the goodbye message as an embed; off sends plain text.'),
      {
        ...messageField('goodbyeMessage', 'Message text', 'The text line. Leave it empty to send only the embed.',
          { maxLength: 2000, nullable: false, trim: false }),
        read: (row) => {
          const m = row?.goodbyeMessage;
          if (typeof m === 'string') return m;
          return row?.goodbyeUseEmbed === true ? '' : DEFAULT_TEXTS.goodbyeMessage;
        },
      },
      { ...welcomeEmbedField('goodbyeEmbed', 'Embed',
        'Shown when "Goodbye message as an embed" is on. Clear it to use the default embed.'),
      showWhen: { field: 'goodbyeUseEmbed', in: [true] } },
    ],
  },
  {
    id: 'autoroles',
    title: 'Autoroles',
    description: 'Roles every new member gets.',
    fields: [
      { field: 'autoroleIds', type: 'roles', label: 'Autoroles', assignable: true, maxItems: LIMITS.AUTOROLES, default: [],
        description: "Given to every member who joins (after verification and Rules Screening). Roles must be below "
          + "the bot's highest role." },
      int('autoroleDelay', 0, 0, NUMBER_LIMITS.autoroleDelayMaxSeconds, 'Delay (seconds)',
        'Wait this long after the join before giving the autoroles (0 = immediately, at most 7 days). Members '
          + 'already waiting keep their time.',
        { unit: 'seconds' }),
    ],
  },
];

// ── Verification ────────────────────────────────────────────────────────────────────────────────────────────

/** panelTemplate (captchaService): the stored panel, or the default when it shows nothing. */
function captchaPanelOf(row) {
  const raw = row?.captchaTemplate;
  if (isPlainObject(raw)) {
    const t = normalizeTemplate(raw);
    if (t.content || t.embed) return t;
  }
  return normalizeTemplate(clone(DEFAULT_CAPTCHA_PANEL));
}

/** parseVerifyButton (captchaService). */
function verifyButtonOf(row) {
  const raw = isPlainObject(row?.captchaTemplate) ? row.captchaTemplate.verifyButton : null;
  const label = typeof raw?.label === 'string' && raw.label.trim() ? raw.label.trim().slice(0, 80) : DEFAULT_VERIFY_BUTTON.label;
  const style = ['SUCCESS', 'PRIMARY', 'SECONDARY', 'DANGER'].includes(raw?.style) ? raw.style : DEFAULT_VERIFY_BUTTON.style;
  return { label, style };
}

function captchaDraft(draft, row) {
  if (!draft.captchaTemplate) {
    const raw = isPlainObject(row?.captchaTemplate) ? normalizeTemplate(row.captchaTemplate) : { content: null, embed: null,
      buttons: [] };
    draft.captchaTemplate = { ...raw, verifyButton: verifyButtonOf(row) };
  }
  return draft.captchaTemplate;
}

const CAPTCHA_GROUPS = [
  {
    id: 'verification',
    title: 'Verification',
    description: 'Post or move the verification panel with /captcha setup or /captcha channel in Discord.',
    fields: [
      { field: 'captchaRoleId', type: 'role', label: 'Verified role', assignable: true, default: null,
        description: 'Given to members who pass. It cannot be a moderator, immune or mute role.' },
      { field: 'captchaMode', type: 'enum', label: 'Captcha type', default: 'IMAGE',
        description: 'Image: type the code from a picture. Button: one click.',
        options: [{ value: 'IMAGE', label: 'Image code' }, { value: 'BUTTON', label: 'Button only' }] },
      int('captchaTimeoutMinutes', 0, 0, NUMBER_LIMITS.captchaTimeoutMaxMinutes, 'Kick unverified members after (minutes)',
        '0 never kicks; otherwise 5 to 10080 minutes (7 days). Setting 0 cancels the kicks already scheduled.', {
          unit: 'minutes',
          check: (n) => (n === 0 || n >= NUMBER_LIMITS.captchaTimeoutMinMinutes ? null
            : 'Use 0 (never) or between 5 and 10080 minutes (7 days).'),
        }),
    ],
  },
  {
    id: 'panel',
    title: 'Verification panel',
    description: 'The message with the Verify button. Saving here also updates the posted panel.',
    fields: [
      {
        field: 'captchaTemplate', type: 'template', label: 'Panel message',
          default: normalizeTemplate(clone(DEFAULT_CAPTCHA_PANEL)),
        description: 'Text, embed and link buttons of the panel. {user} tokens stay as they are: the panel has no member.',
        read: captchaPanelOf,
        write: (value, draft, row) => { Object.assign(captchaDraft(draft, row), normalizeTemplate(value)); },
      },
      {
        field: 'captchaVerifyButton', type: 'object', label: 'Verify button', default: { ...DEFAULT_VERIFY_BUTTON },
        description: 'Label and colour of the button that starts the verification.',
        fields: [
          { field: 'label', type: 'text', label: 'Label', maxLength: 80, minLength: 1, default: DEFAULT_VERIFY_BUTTON.label },
          { field: 'style', type: 'enum', label: 'Colour', default: DEFAULT_VERIFY_BUTTON.style, options: [
            { value: 'SUCCESS', label: 'Green' }, { value: 'PRIMARY', label: 'Blurple' },
            { value: 'SECONDARY', label: 'Grey' }, { value: 'DANGER', label: 'Red' },
          ] },
        ],
        read: verifyButtonOf,
        write: (value, draft, row) => { captchaDraft(draft, row).verifyButton = { ...value }; },
      },
    ],
  },
];

// ── Levels ──────────────────────────────────────────────────────────────────────────────────────────────────

const LEVELING_GROUPS = [
  {
    id: 'xp',
    title: 'XP',
    description: 'Members earn a random amount of XP per message, at most once per cooldown.',
    fields: [
      int('xpMin', 15, 1, NUMBER_LIMITS.xpRangeMax, 'Minimum XP per message', 'MEE6 uses 15.'),
      int('xpMax', 25, 1, NUMBER_LIMITS.xpRangeMax, 'Maximum XP per message', 'MEE6 uses 25. Must be at least the minimum.'),
      int('xpCooldown', 60, 5, 3600, 'Cooldown (seconds)',
        'Messages within this time after the last rewarded one earn nothing.', { unit: 'seconds' }),
      { field: 'xpMultiplier', type: 'float', label: 'Server XP multiplier', default: 1, min: 0.1, max: 10, step: 0.01,
        description: 'Multiplies every XP gain (0.1 to 10).', normalize: round2 },
    ],
  },
  {
    id: 'level-up',
    title: 'Level-up announcement',
    fields: [
      { field: 'levelUpMode', type: 'enum', label: 'Announce level-ups', default: 'CURRENT',
        description: 'Where the level-up message goes.',
        options: [
          { value: 'OFF', label: 'Disabled' }, { value: 'CURRENT', label: 'Current channel' },
          { value: 'DM', label: 'Private message' }, { value: 'CHANNEL', label: 'Custom channel' },
        ] },
      { ...channel('levelUpChannelId', 'Announcement channel', 'Used when "Custom channel" is selected.', { nullable: false }),
        showWhen: { field: 'levelUpMode', in: ['CHANNEL'] } },
      bool('levelUpUseEmbed', false, 'Level-up message as an embed',
        'Sends level-up announcements as an embed; off sends plain text.'),
      messageField('levelUpMessage', 'Level-up message', 'Empty uses the default message.', {
        maxLength: 1000, placeholders: ['{player}', '{player.name}', '{level}', '{rank}', '{xp}', '{mention}'],
      }),
    ],
  },
  {
    id: 'voice-xp',
    title: 'Voice XP',
    fields: [
      bool('voiceXpEnabled', false, 'Voice XP',
        'Members also earn XP for time spent in voice channels (not while alone, muted or AFK). Switching it off '
          + 'drops open voice sessions without credit.'),
      int('voiceXpRate', 5, 1, 50, 'XP per minute in voice', 'How much XP a minute in voice is worth.', { unit: 'XP per minute' }),
    ],
  },
  {
    id: 'no-xp',
    title: 'No-XP roles & channels',
    fields: [
      { field: 'noXpRoleIds', type: 'roles', label: 'No-XP roles', maxItems: LIMITS.NOXP_ROLES, default: [],
        description: 'Members with one of these roles earn no XP.' },
      { field: 'noXpChannelIds', type: 'channels', label: 'No-XP channels', channelTypes: [...ANY_CHANNEL],
        maxItems: LIMITS.NOXP_CHANNELS, default: [],
        description: 'No XP is earned in these channels, their threads, or the channels of a listed category.' },
    ],
  },
  {
    id: 'role-rewards',
    title: 'Role rewards behaviour',
    description: 'Pick the reward roles themselves in the list below. Run /levelconfig role sync in Discord to '
      + 'apply a change to every member.',
    fields: [
      bool('levelRolesStack', true, 'Keep lower reward roles',
        'Members keep earlier reward roles when they earn a higher one; off keeps only the highest.'),
      bool('levelRolesSyncOnDemotion', true, 'Remove rewards on XP loss',
        'Members who lose XP also lose the reward roles above their new level.'),
    ],
  },
  {
    id: 'boosts',
    title: 'XP boost roles',
    fields: [
      {
        field: 'xpBoostRoles', type: 'objectList', label: 'XP boost roles', maxItems: LIMITS.XP_BOOST_ROLES, default: [],
        description: 'Members with one of these roles earn more XP; the highest multiplier applies.',
        itemFields: [
          { field: 'roleId', type: 'role', label: 'Role', required: true, default: null },
          { field: 'multiplier', type: 'float', label: 'Multiplier', min: 1.1, max: 5, step: 0.01, default: 1.5 },
        ],
        read: (row) => (Array.isArray(row?.xpBoostRoles) ? row.xpBoostRoles : []),
        check: (list) => {
          const seen = new Set();
          for (const [i, b] of list.entries()) {
            if (seen.has(b.roleId)) return { path: `${i}.roleId`, message: 'That role is already in the list.' };
            seen.add(b.roleId);
          }
          return null;
        },
        normalize: (list) => list.map((b) => ({ roleId: b.roleId, multiplier: round2(b.multiplier) })),
      },
    ],
  },
  {
    id: 'rank-card',
    title: 'Rank card',
    description: 'The look of the /rank card.',
    fields: [
      {
        field: 'rankCardStyle', type: 'object', label: 'Rank card', default: { ...DEFAULT_RANK_CARD_STYLE },
        description: 'Colours and background of the /rank card.',
        fields: [
          { field: 'background', type: 'color', format: 'hex', label: 'Background colour',
            default: DEFAULT_RANK_CARD_STYLE.background },
          { field: 'backgroundUrl', type: 'url', nullable: true, maxLength: 500, default: null, label: 'Background image',
            description: 'An http(s) link to an image (at most 8 MB). The bot downloads it once to check it.' },
          { field: 'accentColor', type: 'color', format: 'hex', label: 'Accent colour',
            default: DEFAULT_RANK_CARD_STYLE.accentColor },
          { field: 'textColor', type: 'color', format: 'hex', label: 'Text colour', default: DEFAULT_RANK_CARD_STYLE.textColor },
          { field: 'overlayOpacity', type: 'float', min: 0, max: 0.9, step: 0.01, label: 'Overlay opacity',
            default: DEFAULT_RANK_CARD_STYLE.overlayOpacity, description: 'How much the background image is darkened (0 to 0.9).' },
        ],
        normalize: (v) => ({ ...v, overlayOpacity: round2(v.overlayOpacity) }),
      },
    ],
  },
];

// ── Other modules ───────────────────────────────────────────────────────────────────────────────────────────

const BIRTHDAY_GROUPS = [
  {
    id: 'announcement',
    title: 'Birthday announcement',
    fields: [
      channel('birthdayChannelId', 'Announcement channel', 'Where birthdays are announced. Empty turns announcements off.'),
      int('birthdayHour', 9, 0, 23, 'Announcement hour',
        'The hour (0-23) in the server time zone when birthdays are announced.', { unit: 'hour' }),
      messageField('birthdayMessage', 'Message', 'Used when the birth year is unknown. Empty uses the default.',
        { maxLength: 1000 }),
      messageField('birthdayMessageAge', 'Message with age', 'Used when the birth year is known. Empty uses the default.',
        { maxLength: 1000, placeholders: ['{age}'] }),
    ],
  },
  {
    id: 'role',
    title: 'Birthday role',
    fields: [
      { field: 'birthdayRoleId', type: 'role', label: 'Birthday role', nullable: true, assignable: true, default: null,
        description: "Given for the member's birthday and removed the next day. Changing it here does not take the old role off "
          + 'members celebrating today; /birthday config role in Discord does.' },
    ],
  },
  {
    id: 'members',
    title: 'Saving birthdays',
    fields: [bool('birthdayRequireYear', false, 'Require birth year',
      'Members must include the year when they save their birthday.')],
  },
];

const ACHIEVEMENT_GROUPS = [
  {
    id: 'announcement',
    title: 'Unlock announcements',
    description: 'Create and edit the achievements themselves in the list below.',
    fields: [
      { field: 'achievementAnnounceMode', type: 'enum', label: 'Announce unlocks', default: 'CURRENT',
        description: 'Where unlocks are announced. Boosts and purchases have no channel of their own and use the '
          + 'announcement channel.',
        options: [
          { value: 'OFF', label: 'Disabled' }, { value: 'CURRENT', label: 'Current channel' },
          { value: 'DM', label: 'Private message' }, { value: 'CHANNEL', label: 'Custom channel' },
        ] },
      channel('achievementChannelId', 'Announcement channel',
        'Used by "Custom channel", and for unlocks that happen outside a channel.',
        { nullable: false }),
      bool('achievementUseEmbed', false, 'Announcement as an embed',
        'Sends achievement unlock announcements as an embed; off sends plain text.'),
      {
        ...messageField('achievementMessage', 'Announcement message', 'Empty uses the default message.', {
          maxLength: 1500,
          placeholders: ['{player}', '{player.name}', '{achievement}', '{achievement.name}', '{achievement.description}',
            '{tier}', '{tier.emoji}', '{goal}', '{progress}', '{rewards}'],
        }),
        check: (text) => (text !== null && renderPlaceholdersDetailed(text, { vars: SAMPLE_ACHIEVEMENT_VARS }).text.trim() === ''
          ? 'That text renders as an empty message.' : null),
      },
    ],
  },
];

const ECONOMY_GROUPS = [
  {
    id: 'currency',
    title: 'Currency',
    fields: [
      { field: 'currencyName', type: 'text', label: 'Currency name', default: 'coins', maxLength: 32, minLength: 1,
        description: 'Shown after every amount, e.g. "250 coins".',
        pattern: '[^\\r\\n`*_~|<>@#\\\\]+',
        patternMessage: 'Use letters, numbers, spaces and simple punctuation only (no markdown, mentions or newlines).',
        normalize: (v) => v.replace(/\s+/g, ' ') },
      { field: 'currencyEmoji', type: 'emoji', label: 'Currency emoji', default: '🪙',
        description: 'A standard emoji or a custom emoji of this server.',
        check: (v) => (currencyEmojiOk(v) ? null : 'That is not an emoji. Use a standard emoji like 🪙 or a custom emoji.') },
    ],
  },
  {
    id: 'balances',
    title: 'Balances & limits',
    fields: [
      int('economyStartingBalance', 1000, 0, COIN_MAX, 'Starting balance',
        'What a brand-new account starts with. At most the maximum balance.'),
      int('economyMaxBalance', 1_000_000_000, 1, COIN_MAX, 'Maximum balance',
        'No account can hold more. Lowering it also lowers every balance above the new maximum.'),
      int('economyMaxBet', 0, 0, COIN_MAX, 'Maximum bet', 'The highest bet of a casino game. 0 = no limit beyond the balance.'),
      int('economyMaxPayout', 0, 0, COIN_MAX, 'Maximum winnings', 'The highest net win of one casino game. 0 = no limit.'),
      bool('economyImportGlobal', true, 'Copy the legacy global balance',
        "New accounts start once with the member's balance from the old global wallet."),
    ],
  },
  {
    id: 'daily',
    title: 'Daily reward',
    fields: [
      int('dailyMin', 50, 1, NUMBER_LIMITS.economyRangeMax, 'Minimum daily reward', 'The smallest /daily payout.'),
      int('dailyMax', 120, 1, NUMBER_LIMITS.economyRangeMax, 'Maximum daily reward',
        'The largest /daily payout. At least the minimum.'),
    ],
  },
  {
    id: 'work',
    title: 'Work',
    fields: [
      int('workMin', 100, 1, NUMBER_LIMITS.economyRangeMax, 'Minimum work pay', 'The smallest /work payout.'),
      int('workMax', 300, 1, NUMBER_LIMITS.economyRangeMax, 'Maximum work pay', 'The largest /work payout. At least the minimum.'),
      int('workCooldownMinutes', 60, 1, NUMBER_LIMITS.workCooldownMaxMinutes, 'Work cooldown (minutes)',
        'How long a member waits between two /work.', { unit: 'minutes' }),
    ],
  },
  {
    id: 'games',
    title: 'Casino channels',
    fields: [
      { field: 'economyGameChannelMode', type: 'enum', label: 'Casino games are allowed', default: 'ALLOW',
        description: 'With an empty list the games work everywhere.',
        options: [{ value: 'ALLOW', label: 'Only in the listed channels' }, { value: 'DENY',
          label: 'Everywhere except the listed channels' }] },
      { field: 'economyGameChannelIds', type: 'channels', label: 'Channels', channelTypes: ['text', 'announcement', 'voice',
        'forum'],
        maxItems: LIMITS.ECONOMY_GAME_CHANNELS, default: [], description: 'Threads count as their parent channel.' },
    ],
  },
];

/** Every editable field per module (MODULES key -> groups). A module with nothing but its on/off flag has none. */
export const MODULE_GROUPS = {
  moderation: MODERATION_GROUPS,
  automod: AUTOMOD_GROUPS,
  logging: LOGGING_GROUPS,
  welcome: WELCOME_GROUPS,
  captcha: CAPTCHA_GROUPS,
  leveling: LEVELING_GROUPS,
  customcommands: [{
    id: 'triggers',
    title: 'Auto-responders',
    fields: [bool('autoTriggersEnabled', true, 'Auto-responder triggers',
      'Replies automatically when a message matches one of your triggers (needs the Custom Commands module on).')],
  }],
  stats: [{
    id: 'stats',
    title: 'Statistics channels',
    fields: [channel('statsCategoryId', 'Category for new stat channels',
      'New statistics channels are created in this category (the bot needs Manage Channels there). Empty creates them at the top.',
      { kinds: ['category'] })],
  }],
  birthdays: BIRTHDAY_GROUPS,
  achievements: ACHIEVEMENT_GROUPS,
  economy: ECONOMY_GROUPS,
  search: [{
    id: 'urban',
    title: 'Urban Dictionary',
    fields: [bool('urbanNsfwOnly', true, '/search urban only in age-restricted channels',
      'Urban Dictionary lookups only work in age-restricted channels.')],
  }],
  liverole: [{
    id: 'liverole',
    title: 'Live role',
    description: 'Changes here apply as members start or stop streaming. /liverole set and require in Discord also '
      + 'update current streamers at once.',
    fields: [
      { field: 'liveRoleId', type: 'role', label: 'Live role', assignable: true, default: null,
        description: 'Given while a member streams. Use a role only the bot hands out: members without a stream lose it.' },
      { field: 'liveRoleRequiredRoleId', type: 'role', label: 'Only for members with', nullable: true, default: null,
        description: 'Only members with this role get the live role. Empty = everyone who streams.' },
    ],
  }, {
    id: 'announce',
    title: 'Go-live posts',
    description: 'Posted when a member goes live and gets the live role, at most once an hour per member. Only the ping role '
      + 'is notified.',
    fields: [
      channel('liveRoleAnnounceChannelId', 'Post channel', 'Where go-live posts go. Empty turns them off.'),
      messageField('liveRoleAnnounceMessage', 'Message', '{url} is the stream link. Empty uses the default.',
        { maxLength: 1000, placeholders: ['{url}', '{platform}', '{title}', '{game}'] }),
      { field: 'liveRoleAnnouncePingRoleId', type: 'role', label: 'Role to ping', nullable: true, default: null,
        description: 'Mentioned at the start of each post. It only pings if the role is mentionable or the bot has Mention Everyone.' },
    ],
  }],
  counting: [{
    id: 'counting',
    title: 'Counting channel',
    description: 'A new channel starts from 1 at the next count. /counting set in Discord also posts the rules there.',
    fields: [
      channel('countingChannelId', 'Counting channel', 'Members count up here, one number per message.'),
    ],
  }],
  autotime: [{
    id: 'autotime',
    title: 'Ignored channels',
    description: 'Times are read in the zone of the member who typed them: their own (/timezone set) or their timezone role.',
    fields: [
      { field: 'autoTimeIgnoredChannelIds', type: 'channels', label: 'Ignored channels', channelTypes: [...ANY_CHANNEL],
        maxItems: LIMITS.AUTOTIME_IGNORED_CHANNELS, default: [],
        description: 'Times typed in these channels are never answered. A category covers its channels and threads.' },
    ],
  }],
  suggestions: [{
    id: 'suggestions',
    title: 'Suggestions',
    description: '/suggestion submit posts here; members vote with the buttons and staff review with /suggestion.',
    fields: [
      channel('suggestionChannelId', 'Suggestion channel', 'New suggestions are posted here with voting buttons.'),
      bool('suggestionThreads', true, 'Open a thread on each suggestion',
        'Starts a discussion thread on every new suggestion (the bot needs Create Public Threads).'),
      bool('suggestionAllowAnonymous', false, 'Allow anonymous suggestions',
        'Members may hide their name on a suggestion. Staff can still look up who wrote it.'),
      bool('suggestionDmOnUpdate', true, 'DM the author on review',
        'The author gets a direct message when staff approve, deny or otherwise update their suggestion.'),
      int('suggestionCooldownSeconds', 300, 0, 86400, 'Cooldown between suggestions',
        'Seconds a member waits between two suggestions. 0 = no cooldown.', { unit: 'seconds' }),
    ],
  }],
  modmail: [{
    id: 'modmail',
    title: 'Modmail',
    description: 'Members DM the bot; each conversation opens a thread in the staff channel. Block members with /modmail block.',
    fields: [
      channel('modmailChannelId', 'Staff channel',
        'Conversations open as threads here. Keep it hidden from members; the bot needs Create Private Threads.'),
      { field: 'modmailPingRoleIds', type: 'roles', label: 'Roles to ping', maxItems: LIMITS.MODMAIL_PING_ROLES, default: [],
        description: 'Pinged in the new thread when a conversation opens.' },
      messageField('modmailGreeting', 'Greeting', 'Sent to the member when their conversation opens.', { maxLength: 1000 }),
      bool('modmailAnonymousReplies', false, 'Anonymous staff replies',
        'Replies reach the member as "Staff" instead of the name of the staff member.'),
    ],
  }],
  afk: [{
    id: 'afk',
    title: 'AFK',
    description: 'Members go AFK with /afk set; it clears on their next message.',
    fields: [
      bool('afkNicknamePrefix', true, 'Add [AFK] to nicknames',
        'Puts "[AFK] " in front of the nickname while a member is AFK (the bot needs Manage Nicknames).'),
      { field: 'afkIgnoredChannelIds', type: 'channels', label: 'Ignored channels', channelTypes: [...ANY_CHANNEL],
        maxItems: LIMITS.AFK_IGNORED_CHANNELS, default: [],
        description: 'Messages here neither end an AFK nor answer mentions of AFK members. A category covers its channels.' },
    ],
  }],
  antiraid: [{
    id: 'detection',
    title: 'Raid detection',
    description: 'A raid is this many joins within this many seconds.',
    fields: [
      int('antiRaidJoinThreshold', 10, 3, 100, 'Joins', 'How many joins in the window count as a raid.'),
      int('antiRaidJoinSeconds', 10, 3, 300, 'Window', 'The window the joins are counted in.', { unit: 'seconds' }),
      {
        field: 'antiRaidAction', type: 'enum', label: 'Members who join during a raid', default: 'ALERT',
        description: 'What happens to members who join while a raid lockdown lasts.',
        options: [
          { value: 'ALERT', label: 'Only alert the moderators' },
          { value: 'KICK', label: 'Kick them' },
          { value: 'BAN', label: 'Ban them' },
        ],
      },
      channel('antiRaidAlertChannelId', 'Alert channel', 'Raid alerts go here. Empty uses the moderation log channel.'),
    ],
  }, {
    id: 'lockdown',
    title: 'Lockdown',
    description: 'What the bot does while a raid lasts. /antiraid lockdown starts or ends one by hand.',
    fields: [
      bool('antiRaidPauseInvites', true, 'Pause invites',
        'Pauses every invite while the lockdown lasts (the bot needs Manage Server).'),
      bool('antiRaidLockChat', false, 'Lock chat',
        'Takes Send Messages from @everyone while the lockdown lasts and gives it back afterwards (the bot needs Manage Roles).'),
      int('antiRaidLockdownMinutes', 30, 5, 1440, 'Lockdown length', 'The lockdown lifts itself after this long.', { unit: 'minutes' }),
    ],
  }, {
    id: 'account-age',
    title: 'New accounts',
    fields: [
      int('antiRaidMinAccountAgeDays', 0, 0, 365, 'Minimum account age',
        'Accounts younger than this get the action below. 0 = off.', { unit: 'days' }),
      {
        field: 'antiRaidYoungAction', type: 'enum', label: 'Action for new accounts', default: 'KICK',
        description: 'What happens to accounts younger than the minimum age when they join.',
        options: [
          { value: 'ALERT', label: 'Only alert the moderators' },
          { value: 'KICK', label: 'Kick them' },
          { value: 'BAN', label: 'Ban them' },
        ],
      },
    ],
  }],
  events: [{
    id: 'events',
    title: 'Events',
    fields: [
      channel('eventChannelId', 'Announcement channel', 'Where /event create posts. Empty posts in the channel the command runs in.'),
      bool('eventCreateDiscordEvent', true, 'Also create a Discord event',
        'Adds each event to the Events list of the server too (the bot needs Manage Events).'),
      int('eventReminderMinutes', 15, 0, 1440, 'Reminder',
        'Minutes before the start to ping members who are going. 0 = no reminder.', { unit: 'minutes' }),
    ],
  }],
  rolepersist: [{
    id: 'rolepersist',
    title: 'Role persistence',
    description: 'Roles with powerful permissions, managed roles and roles above the bot are never given back.',
    fields: [
      bool('rolePersistNicknames', true, 'Give back nicknames', 'Also restores the nickname a member had when they left.'),
      { field: 'rolePersistIgnoredRoleIds', type: 'roles', label: 'Never give back', maxItems: LIMITS.ROLEPERSIST_IGNORED_ROLES,
        default: [], description: 'These roles are not saved when a member leaves.' },
      int('rolePersistDays', 90, 1, 365, 'Keep for', 'Saved roles of members who left longer ago than this are dropped.',
        { unit: 'days' }),
    ],
  }],
  boosts: [{
    id: 'boost-thanks',
    title: 'Boost thank-you',
    fields: [
      channel('boostChannelId', 'Thank-you channel', 'A thank-you is posted here when a member boosts the server. Empty = none.'),
      messageField('boostMessage', 'Thank-you message', 'Posted when a member starts boosting.',
        { maxLength: 2000, placeholders: ['{boosts}', '{tier}'] }),
    ],
  }, {
    id: 'booster-roles',
    title: 'Booster roles',
    description: 'Boosters make a role of their own with /boost role. Booster roles never get any permissions.',
    fields: [
      bool('boostCustomRoles', false, 'Let boosters make their own role',
        'Boosters can create a role with their own name, colour and icon (the bot needs Manage Roles).'),
      { field: 'boostRoleAnchorId', type: 'role', label: 'Place booster roles below', nullable: true, assignable: true,
        default: null, description: 'Booster roles go just below this role. Empty = just above the Nitro Booster role (or at the bottom), '
          + 'so moderators keep power over boosters.' },
      int('boostRoleGraceDays', 0, 0, 30, 'Keep the role after a boost ends',
        'Days a member keeps their role after they stop boosting. 0 = removed at once.', { unit: 'days' }),
    ],
  }],
  backups: [{
    id: 'backups',
    title: 'Automatic backups',
    description: 'Make, restore and download backups with /backup. Only the server owner can restore or delete a backup.',
    fields: [
      int('backupIntervalHours', 0, 0, 720, 'Back up every', 'Hours between automatic backups. 0 = off.', { unit: 'hours' }),
      int('backupKeep', 7, 1, LIMITS.BACKUPS_AUTO_KEEP, 'Automatic backups kept',
        'When a new automatic backup would go over this, the oldest one is deleted.'),
    ],
  }],
  confessions: [{
    id: 'confessions',
    title: 'Confessions',
    description: 'Members confess with /confession submit. Staff never see who wrote one; only admins can reveal an author '
      + 'with /confession reveal, and that is written to the moderation log.',
    fields: [
      channel('confessionChannelId', 'Confession channel', 'Confessions are posted here.'),
      channel('confessionReviewChannelId', 'Review channel',
        'Confessions wait here for a moderator to approve them. Empty = posted at once. Keep it hidden from members.'),
      bool('confessionAllowImages', false, 'Allow an image', 'Members may attach one image to a confession.'),
      bool('confessionBlockLinks', true, 'Refuse links', 'Confessions that contain a link are refused.'),
      int('confessionCooldownSeconds', 300, 0, 86400, 'Cooldown between confessions',
        'Seconds a member waits between two confessions. 0 = no cooldown.', { unit: 'seconds' }),
    ],
  }],
  autothreads: [{
    id: 'auto-threads',
    title: 'Auto threads',
    description: 'Every member message in these channels gets its own thread (the bot needs Create Public Threads).',
    fields: [
      {
        field: 'autoThreadChannels', type: 'objectList', label: 'Channels', maxItems: LIMITS.AUTOTHREAD_CHANNELS, default: [],
        description: 'Thread names may use {user} (display name), {content} (the start of the message), {channel} and {date}.',
        itemFields: [
          { field: 'channelId', type: 'channel', label: 'Channel', required: true, channelTypes: ['text', 'announcement'], default: null },
          { field: 'name', type: 'text', label: 'Thread name', maxLength: 100, minLength: 1, default: '{content}' },
          {
            field: 'archiveMinutes', type: 'enum', label: 'Archive after', default: '1440',
            options: [
              { value: '60', label: '1 hour without messages' },
              { value: '1440', label: '1 day without messages' },
              { value: '4320', label: '3 days without messages' },
              { value: '10080', label: '1 week without messages' },
            ],
          },
          { field: 'closeButton', type: 'boolean', label: 'Close button', default: false,
            description: 'Posts a button in each thread that its starter or a moderator can use to close it.' },
        ],
        read: (row) => (Array.isArray(row?.autoThreadChannels) ? row.autoThreadChannels : []),
        check: (list) => {
          const seen = new Set();
          for (const [i, l] of list.entries()) {
            if (seen.has(l.channelId)) return { path: `${i}.channelId`, message: 'That channel is already in the list.' };
            seen.add(l.channelId);
          }
          return null;
        },
      },
    ],
  }, {
    id: 'auto-publish',
    title: 'Auto publish',
    fields: [
      { field: 'autoPublishChannelIds', type: 'channels', label: 'Announcement channels', channelTypes: ['announcement'],
        maxItems: LIMITS.AUTOPUBLISH_CHANNELS, default: [],
        description: 'New posts here, by members, bots or webhooks, are published to the servers that follow the channel '
          + '(the bot needs Manage Messages). Discord allows 10 per channel per hour; the rest stay unpublished.' },
    ],
  }],
  voiceroles: [{
    id: 'voice-roles',
    title: 'Voice roles',
    description: 'Use roles made only for this: the bot takes them away from anyone who is not in a linked voice channel.',
    fields: [
      {
        field: 'voiceRoleLinks', type: 'objectList', label: 'Channel roles', maxItems: LIMITS.VOICE_ROLE_LINKS, default: [],
        description: 'Members in the channel, or in any voice channel of the category, have the role.',
        itemFields: [
          { field: 'channelId', type: 'channel', label: 'Voice channel or category', required: true,
            channelTypes: ['voice', 'stage', 'category'], default: null },
          { field: 'roleId', type: 'role', label: 'Role', required: true, assignable: true, default: null },
        ],
        read: (row) => (Array.isArray(row?.voiceRoleLinks) ? row.voiceRoleLinks : []),
        check: (list) => {
          const seen = new Set();
          for (const [i, l] of list.entries()) {
            const key = `${l.channelId}:${l.roleId}`;
            if (seen.has(key)) return { path: `${i}.roleId`, message: 'That channel already has this role.' };
            seen.add(key);
          }
          return null;
        },
      },
      { field: 'voiceRoleAnyRoleIds', type: 'roles', label: 'Roles for any voice channel', maxItems: LIMITS.VOICE_ROLE_ANY,
        assignable: true, default: [], description: 'Members in any voice channel of the server have these roles.' },
      bool('voiceRoleIgnoreAfk', true, 'None in the AFK channel', 'Members in the server\'s AFK channel get no voice roles.'),
    ],
  }],
  bumps: [{
    id: 'bump-reminders',
    title: 'Bump reminders',
    description: 'Works with Disboard: the bot sees each successful /bump and reminds the server when it can bump again.',
    fields: [
      channel('bumpChannelId', 'Reminder channel', 'Reminders are posted here. Empty = the channel of the last bump.'),
      { field: 'bumpRoleId', type: 'role', label: 'Role to ping', nullable: true, default: null,
        description: 'Pinged with each reminder. Empty = no ping.' },
      messageField('bumpReminderMessage', 'Reminder message', 'Posted when the server can be bumped again.', { maxLength: 1000 }),
      bool('bumpThanks', true, 'Thank the bumper', 'Replies with a thank-you right after a successful bump.'),
      messageField('bumpThanksMessage', 'Thank-you message', 'Sent after each successful bump.',
        { maxLength: 1000, placeholders: ['{next}', '{count}'] }),
      int('bumpRewardCoins', 0, 0, 100000, 'Coins per bump', 'Coins given for each bump while Economy is on. 0 = none.'),
    ],
  }],
};
for (const key of MODULE_NAMES) MODULE_GROUPS[key] ??= [];

// ── The registry ────────────────────────────────────────────────────────────────────────────────────────────

/** Module flags (the on/off switch of each module page). */
const FLAG_FIELDS = MODULE_NAMES.map((key) => bool(MODULES[key].flag, MODULES[key].default === true, MODULES[key].label,
  `Turns the ${MODULES[key].label} module on or off.`));

const groupFields = (groups) => groups.flatMap((g) => g.fields);

/** field -> descriptor, for every field the dashboard reads and writes (general, every module group, module flags). */
export const SETTINGS_FIELDS = Object.freeze(Object.fromEntries([
  ...FLAG_FIELDS, ...groupFields(GENERAL_GROUPS), ...MODULE_NAMES.flatMap((key) => groupFields(MODULE_GROUPS[key])),
].map((d) => [d.field, d])));

/** field -> the GuildSettings column it is stored in (a virtual field names its JSON column, logEvents its inverse list). */
export const SETTINGS_COLUMNS = Object.freeze(Object.fromEntries(Object.keys(SETTINGS_FIELDS).map((field) => [field,
  Object.hasOwn(AUTOMOD_LIST_KEYS, field) || field === HONEYPOT_FIELD.field || Object.values(RULE_FIELDS).some((d) => d.field === field)
    ? 'automodConfig'
    : field === 'captchaVerifyButton' ? 'captchaTemplate'
      : field === 'logEvents' ? 'disabledLogEvents'
        : field])));

/** Columns stored as Json (null clears them with Prisma.DbNull). */
export const JSON_COLUMNS = Object.freeze(['automodConfig', 'infractionEscalation', 'welcomeEmbed', 'welcomeCardStyle',
  'welcomeDmEmbed', 'goodbyeEmbed', 'captchaTemplate', 'xpBoostRoles', 'rankCardStyle']);

/**
 * GuildSettings columns the dashboard never writes, with the reason (test/dashboard.test.js checks that every column is
 * either here or behind a field).
 */
export const NOT_EXPOSED = Object.freeze({
  id: 'row identity',
  guildId: 'row identity',
  createdAt: 'row metadata',
  updatedAt: 'row metadata',
  levelingEnabled: 'deprecated and never read (xpEnabled is the Levels flag)',
  xpRate: 'legacy: only the migration to xpMin/xpMax read it',
  captchaChannelId: 'changes only together with the posted panel: /captcha setup or /captcha channel posts it',
  captchaMessageId: 'bot-managed: the id of the posted verification panel',
  captchaLockdownPrior: 'bot-managed: what /captcha lock off restores',
  modmailBlockedUserIds: 'a list of members: /modmail block and unblock manage it',
  confessionBlockedUserIds: 'secret: blocked by confession number so staff never learn who wrote it (/confession block)',
});

for (const desc of Object.values(SETTINGS_FIELDS)) deepFreeze(desc);
for (const groups of [GENERAL_GROUPS, ...Object.values(MODULE_GROUPS)]) deepFreeze(groups);
Object.freeze(MODULE_GROUPS);

function deepFreeze(v) {
  if (v && typeof v === 'object' && !Object.isFrozen(v)) {
    Object.freeze(v);
    for (const child of Object.values(v)) deepFreeze(child);
  }
  return v;
}

// ── Escalation tiers (normalizeEscalationTiers in src/services/moderationService.js) ───────────────────────

const toPosInt = (v) => (Number.isInteger(v) && v > 0 ? v : null);
const tierKey = (t) => `${t.warnCount}|${t.withinSeconds ?? 'null'}`;
const LEGACY_ESCALATION_SINCE = '2026-10-10T12:00:00.000Z';

/** The stored tiers in the bot's canonical form (legacy rows read, sorted, deduplicated, capped). Never throws. */
export function normalizeEscalation(raw) {
  try {
    if (!Array.isArray(raw)) return [];
    const entries = [];
    for (const e of raw) {
      if (!isPlainObject(e)) continue;
      const legacy = !('withinSeconds' in e) && !('durationSeconds' in e);
      if (!Number.isInteger(e.warnCount) || e.warnCount < 1 || e.warnCount > 100) continue;
      let action = String(e.action ?? '').toUpperCase();
      if (!TIER_ACTIONS.some((a) => a.value === action)) continue;
      let dur = toPosInt(legacy ? e.duration : e.durationSeconds);
      let within = null;
      if (!legacy) {
        const w = Number(e.withinSeconds);
        within = Number.isInteger(w) && w >= 60 && w <= NUMBER_LIMITS.timedActionMaxSeconds ? w : null;
      }
      if (legacy && action === 'BAN' && dur) action = 'TEMPBAN';
      if (action === 'TIMEOUT' || action === 'TEMPBAN') {
        if (dur === null) continue;
        dur = Math.min(Math.max(dur, 60),
          action === 'TIMEOUT' ? NUMBER_LIMITS.timeoutMaxSeconds : NUMBER_LIMITS.timedActionMaxSeconds);
      } else if (action === 'MUTE') {
        if (dur !== null) dur = Math.min(Math.max(dur, 60), NUMBER_LIMITS.timedActionMaxSeconds);
      } else {
        dur = null;
      }
      const cf = e.countFrom instanceof Date ? e.countFrom.toISOString() : e.countFrom;
      let countFrom = typeof cf === 'string' && Number.isFinite(Date.parse(cf)) ? cf : null;
      if (legacy && countFrom === null) countFrom = LEGACY_ESCALATION_SINCE;
      entries.push({ id: Number.isInteger(e.id) && e.id >= 1 ? e.id : null, warnCount: e.warnCount, withinSeconds: within,
        action, durationSeconds: dur, countFrom });
    }
    const taken = new Set();
    let next = Math.max(0, ...entries.map((t) => t.id ?? 0));
    for (const t of entries) {
      if (t.id !== null && !taken.has(t.id)) { taken.add(t.id); continue; }
      do { next += 1; } while (taken.has(next));
      t.id = next;
      taken.add(next);
    }
    const byKey = new Map();
    for (const t of entries) byKey.set(tierKey(t), t);
    return sortTiers([...byKey.values()]).slice(0, LIMITS.ESCALATION_TIERS);
  } catch {
    return [];
  }
}

const sortTiers = (list) => list.sort((a, b) => a.warnCount - b.warnCount
  || (a.withinSeconds ?? Infinity) - (b.withinSeconds ?? Infinity) || a.id - b.id);

/** The /modconfig escalation add rules for each tier, and no two tiers for the same warnings and window. */
function escalationProblem(tiers) {
  const seen = new Set();
  for (const [i, t] of tiers.entries()) {
    const key = tierKey(t);
    if (seen.has(key)) return { path: `${i}.warnCount`, message: 'Another step already uses this number of warnings and window.' };
    seen.add(key);
    const d = t.durationSeconds;
    if (t.action === 'TIMEOUT' && (d === null || d > NUMBER_LIMITS.timeoutMaxSeconds)) {
      return { path: `${i}.durationSeconds`, message: 'A timeout needs a duration of 1 minute to 28 days (2419200 seconds).' };
    }
    if (t.action === 'TEMPBAN' && d === null) return { path: `${i}.durationSeconds`, message: 'A temporary ban needs a duration.' };
    if ((t.action === 'KICK' || t.action === 'BAN') && d !== null) {
      return { path: `${i}.durationSeconds`, message: 'Kick and permanent ban have no duration: leave it empty.' };
    }
  }
  return null;
}

/** Tiers to store: a tier with the same warnings and window keeps its id and countFrom; a new one gets the next id. */
function escalationToStored(tiers, row) {
  const current = normalizeEscalation(row?.infractionEscalation);
  const byKey = new Map(current.map((t) => [tierKey(t), t]));
  let next = Math.max(0, ...current.map((t) => t.id));
  const out = tiers.map((t) => {
    const old = byKey.get(tierKey(t));
    return {
      id: old ? old.id : ++next,
      warnCount: t.warnCount,
      withinSeconds: t.withinSeconds,
      action: t.action,
      durationSeconds: t.action === 'KICK' || t.action === 'BAN' ? null : t.durationSeconds,
      countFrom: old ? old.countFrom : null,
    };
  });
  return sortTiers(out);
}

// ── Reading, validating, storing ────────────────────────────────────────────────────────────────────────────

const storedOf = (desc, row) => (desc.read ? desc.read(row ?? {}) : row?.[desc.field]);

/**
 * Every field's current value, as the browser edits it.
 * @param {object|null|undefined} row a GuildSettings row (settingsBridge.readGuildSettings upgrades legacy JSON first)
 * @returns {Record<string, unknown>}
 */
export function readSettingsValues(row) {
  const values = {};
  for (const [field, desc] of Object.entries(SETTINGS_FIELDS)) values[field] = readValue(desc, storedOf(desc, row));
  return values;
}

/**
 * Every channel and role id the stored settings hold (checkRefs' keep sets: an id saved before may stay even when it was
 * deleted since).
 * @param {object|null|undefined} row
 * @returns {{ channels: Set<string>, roles: Set<string> }}
 */
export function collectSettingsRefs(row) {
  const out = { channels: new Set(), roles: new Set() };
  for (const desc of Object.values(SETTINGS_FIELDS)) collectStoredRefs(desc, storedOf(desc, row), out);
  return out;
}

/**
 * Which stored id sits in which setting (refKey per field path): a role may keep failing a giving check only where
 * it was already stored, never after a move into a field that hands it out.
 * @param {object|null} row
 * @returns {Set<string>}
 */
export function collectSettingsRefKeys(row) {
  const out = new Set();
  for (const desc of Object.values(SETTINGS_FIELDS)) storedRefKeys(desc, storedOf(desc, row), desc.field, out);
  return out;
}

const FORBIDDEN_KEYS = new Set(['__proto__', 'constructor', 'prototype']);
const join = (path, sub) => (sub === undefined || sub === null || sub === '' ? path : `${path}.${sub}`);

/**
 * Validates a PATCH body `{ changes: { [field]: value } }` with the commands' rules for each value (cross-field rules
 * need the stored row: checkCrossFields).
 * @param {unknown} body parsed JSON
 * @returns {{ ok: true, changes: Record<string, unknown>, refs: import('./schema.js').Refs }
 *   | { ok: false, message: string, path?: string }}
 */
export function validateSettingsChanges(body) {
  if (!isPlainObject(body)) return { ok: false, message: 'The body must be a JSON object.' };
  if (Object.keys(body).some((k) => k !== 'changes')) return { ok: false, message: 'The body may only contain "changes".' };
  const input = body.changes;
  if (!isPlainObject(input)) return { ok: false, message: '"changes" must be an object of field: value.' };
  const keys = Object.keys(input);
  if (keys.length === 0) return { ok: false, message: '"changes" is empty.' };
  if (keys.length > MAX_SETTINGS_CHANGES) return { ok: false, message: `At most ${MAX_SETTINGS_CHANGES} changes per request.` };

  const refs = emptyRefs();
  const changes = {};
  for (const key of keys) {
    if (FORBIDDEN_KEYS.has(key) || !Object.hasOwn(SETTINGS_FIELDS, key)) {
      return { ok: false, message: `"${key.slice(0, 64)}" is not a setting the dashboard can change.`, path: key.slice(0, 64) };
    }
    const desc = SETTINGS_FIELDS[key];
    try {
      let value = validateValue(desc, input[key], key, refs);
      if (value !== null || !desc.nullable) {
        const problem = desc.check ? desc.check(value) : null;
        if (problem) {
          throw typeof problem === 'string' ? new FieldError(key, problem) : new FieldError(join(key, problem.path),
            problem.message);
        }
        if (desc.normalize) value = desc.normalize(value);
      }
      changes[key] = value;
    } catch (err) {
      if (err instanceof FieldError) return { ok: false, message: err.message, path: err.path || key };
      throw err;
    }
  }
  return { ok: true, changes, refs };
}

/**
 * The columns to write for validated changes, merged into the stored row where a field is part of a JSON column
 * (automod rules and lists, the captcha panel) or keeps bot-managed parts (escalation tier ids). JSON nulls stay plain
 * null: the caller maps them to Prisma.DbNull (JSON_COLUMNS).
 * @param {Record<string, unknown>} changes from validateSettingsChanges
 * @param {object|null} row the stored row (inside the write lock)
 * @returns {Record<string, unknown>}
 */
export function toStoredChanges(changes, row = null) {
  const draft = {};
  for (const [field, value] of Object.entries(changes)) {
    const desc = SETTINGS_FIELDS[field];
    if (!desc) continue;
    if (desc.write) desc.write(clone(value), draft, row);
    else draft[field] = clone(value);
  }
  delete draft[TOUCHED];
  return draft;
}

/**
 * Rules over several columns, as the commands enforce them. A rule is checked when one of its `fields` is changed; the
 * first changed one (in this order) is blamed.
 */
const CROSS_RULES = [
  {
    fields: ['xpMax', 'xpMin'],
    test: (r) => !(Number.isInteger(r.xpMin) && Number.isInteger(r.xpMax) && r.xpMin > r.xpMax),
    message: 'The minimum XP per message cannot be higher than the maximum.',
  },
  {
    fields: ['dailyMax', 'dailyMin'],
    test: (r) => !(Number.isInteger(r.dailyMin) && Number.isInteger(r.dailyMax) && r.dailyMin > r.dailyMax),
    message: 'The minimum daily reward cannot be higher than the maximum.',
  },
  {
    fields: ['workMax', 'workMin'],
    test: (r) => !(Number.isInteger(r.workMin) && Number.isInteger(r.workMax) && r.workMin > r.workMax),
    message: 'The minimum work pay cannot be higher than the maximum.',
  },
  {
    fields: ['economyStartingBalance', 'economyMaxBalance'],
    test: (r) => !(Number.isInteger(r.economyStartingBalance) && Number.isInteger(r.economyMaxBalance)
      && r.economyStartingBalance > r.economyMaxBalance),
    message: 'The starting balance cannot be higher than the maximum balance.',
  },
  {
    fields: ['levelUpChannelId', 'levelUpMode'],
    test: (r) => r.levelUpMode !== 'CHANNEL' || isSnowflake(r.levelUpChannelId),
    message: 'Pick the announcement channel for "Custom channel".',
  },
  {
    fields: ['achievementChannelId', 'achievementAnnounceMode'],
    test: (r) => r.achievementAnnounceMode !== 'CHANNEL' || isSnowflake(r.achievementChannelId),
    message: 'Pick the announcement channel for "Custom channel".',
  },
  {
    // checkVerifiedRole (captchaService): checked when the verified role is picked.
    fields: ['captchaRoleId'],
    test: (r) => !r.captchaRoleId || !((r.modRoleIds ?? []).includes(r.captchaRoleId)
      || (r.immuneRoleIds ?? []).includes(r.captchaRoleId) || r.muteRoleId === r.captchaRoleId),
    message: 'The verified role cannot also be a moderator, immune or mute role.',
  },
  {
    fields: ['liveRoleRequiredRoleId', 'liveRoleId'],
    test: (r) => !r.liveRoleId || r.liveRoleId !== r.liveRoleRequiredRoleId,
    message: 'The live role and the required role must be different roles.',
  },
];

/**
 * The first cross-field rule the merged row breaks, or null. Only rules that involve a changed field are checked, so an
 * old inconsistency never blocks an unrelated save.
 * @param {object} row the stored row with the changes applied (toStoredChanges)
 * @param {Record<string, unknown>|null} [changes] the validated changes of the request (null: check every rule)
 * @returns {{ path: string, message: string } | null}
 */
export function checkCrossFields(row, changes = null) {
  const r = row ?? {};
  const changed = (field) => changes === null || Object.hasOwn(changes, field);
  for (const rule of CROSS_RULES) {
    const blamed = rule.fields.find(changed);
    if (blamed && !rule.test(r)) return { path: blamed, message: rule.message };
  }

  // /modconfig escalation add: a mute step without a duration, or longer than a timeout can last, needs the mute role.
  if (changed('infractionEscalation') && !isSnowflake(r.muteRoleId)) {
    const sent = Array.isArray(changes?.infractionEscalation) ? changes.infractionEscalation
      : normalizeEscalation(r.infractionEscalation);
    const i = sent.findIndex((t) => t.action === 'MUTE'
      && (t.durationSeconds === null || t.durationSeconds > NUMBER_LIMITS.timeoutMaxSeconds));
    if (i !== -1) {
      return { path: `infractionEscalation.${i}.action`,
        message: 'A mute without a duration or longer than 28 days needs a mute role. Set one first.' };
    }
  }

  // A domain cannot be both blocked and allowed (only possible when both lists are saved together).
  if (changed('automodBlockedDomains') || changed('automodAllowedDomains')) {
    const cfg = automodConfigOf(r);
    const both = cfg.blockedDomains.find((d) => cfg.allowedDomains.includes(d));
    if (both) return { path: 'automodAllowedDomains', message: `${both} is in both the blocked and the allowed domains.` };
  }
  return null;
}
