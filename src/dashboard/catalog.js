// What the dashboard shows and may change: module categories, one-line descriptions and the boolean
// sub-settings of each module. Pure: imports only the module registry.
import { MODULES, MODULE_NAMES } from '../config/modules.js';

/** The dashboard's groups, in display order. Every MODULES key appears in exactly one. */
export const CATEGORIES = Object.freeze([
  Object.freeze({
    id: 'safety',
    label: 'Moderation & Safety',
    description: 'Keep the server safe with moderation tools, automatic filters, audit logs and member verification.',
    modules: Object.freeze(['moderation', 'automod', 'logging', 'captcha']),
  }),
  Object.freeze({
    id: 'community',
    label: 'Community',
    description: 'Greet members, hand out roles and run the features your community uses every day.',
    modules: Object.freeze([
      'welcome', 'reactionroles', 'invites', 'tickets', 'tempvoice', 'stats', 'birthdays', 'polls', 'giveaways', 'starboard',
    ]),
  }),
  Object.freeze({
    id: 'engagement',
    label: 'Levels & Economy',
    description: 'Reward activity with XP, achievements, coins and games.',
    modules: Object.freeze(['leveling', 'achievements', 'economy', 'games']),
  }),
  Object.freeze({
    id: 'tools',
    label: 'Content & Utilities',
    description: 'Custom commands, automations, scheduled messages, feeds and other handy tools.',
    modules: Object.freeze(['customcommands', 'automations', 'embeds', 'reminders', 'feeds', 'translation', 'search', 'liverole']),
  }),
]);

/** One user-facing sentence per module. */
export const DESCRIPTIONS = Object.freeze({
  moderation: 'Ban, kick, timeout and warn with case history and escalation.',
  automod: 'Automatically removes or punishes bad words, spam, invites, links, caps, mass mentions and more.',
  logging: 'Posts message, member, voice, channel, role and server events to your log channels.',
  welcome: 'Greets new members with a message, a welcome card or a DM, says goodbye and gives autoroles.',
  captcha: 'New members solve a captcha before they get access; those who never verify can be kicked.',
  reactionroles: 'Members pick their own roles from buttons, a dropdown or emoji reactions.',
  leveling: 'Members earn XP for chatting and voice time, level up and unlock reward roles.',
  customcommands: 'Your own !commands and /tag replies, plus keyword auto-responders.',
  automations: 'When-If-Do rules that react to messages, reactions, voice activity, joins and threads.',
  translation: "Translates a channel's messages into up to five languages.",
  invites: 'Tracks who invited whom, with an invite leaderboard and personal invite links.',
  embeds: 'Build, save, publish and edit rich embed messages with fields and link buttons.',
  reminders: "Scheduled channel messages at a time of day, on an interval or once, in the server's time zone.",
  giveaways: 'Button-entry giveaways with role requirements, weighted odds, rerolls and XP or coin prizes.',
  polls: 'Single or multiple choice polls with live results and an end time.',
  starboard: 'Reposts popular messages to a showcase channel once they collect enough reactions.',
  tickets: 'Support ticket panels with categories, staff claiming and HTML transcripts.',
  tempvoice: 'Join-to-create voice channels that their owners can lock, rename and resize.',
  stats: 'Channel names that count members, bots, online members, boosts and more.',
  birthdays: 'Members save their birthday and the bot announces it, with an optional birthday role.',
  achievements: 'Tiered achievements for messages, reactions, levels, boosts and purchases, with rewards.',
  economy: 'A wallet per server with daily rewards, work, payments, a shop and casino games.',
  games: 'Rock-paper-scissors, number guessing, tic-tac-toe and two-player Scrabble.',
  feeds: 'Posts new RSS, YouTube, Reddit, Bluesky and podcast items to a channel.',
  search: 'Look up anime, manga, Pokémon and Urban Dictionary definitions.',
  liverole: 'Gives members a role while they are streaming live.',
});

const sub = (field, defaultValue, label, description) => Object.freeze({ field, label, description, default: defaultValue });

/**
 * Boolean GuildSettings columns shown inside each module (defaults match prisma/schema.prisma).
 * Never exposed: levelingEnabled (deprecated), captchaLockdownPrior (bot-managed state).
 */
export const SUB_TOGGLES = Object.freeze({
  moderation: Object.freeze([
    sub('modDmTargets', true, 'DM members when punished', 'Members get a direct message when they are warned, timed out, kicked or banned.'),
  ]),
  welcome: Object.freeze([
    sub('welcomeMessageEnabled', true, 'Welcome message in a channel', 'Posts the welcome message in the welcome channel when someone joins.'),
    sub('welcomeUseEmbed', true, 'Welcome message as an embed', 'Sends the channel welcome as an embed; off sends plain text.'),
    sub('welcomeDmEnabled', false, 'Welcome DM', 'Also sends new members a private welcome message.'),
    sub('welcomeDmCard', false, 'Welcome card in the DM', 'Attaches the welcome card image to the welcome DM.'),
    sub('goodbyeEnabled', false, 'Goodbye message', 'Posts a message in the goodbye channel when a member leaves.'),
    sub('goodbyeUseEmbed', false, 'Goodbye message as an embed', 'Sends the goodbye message as an embed; off sends plain text.'),
  ]),
  leveling: Object.freeze([
    sub('voiceXpEnabled', false, 'Voice XP', 'Members also earn XP for time spent in voice channels.'),
    sub('levelUpUseEmbed', false, 'Level-up message as an embed', 'Sends level-up announcements as an embed; off sends plain text.'),
    sub('levelRolesStack', true, 'Keep lower reward roles', 'Members keep earlier reward roles when they earn a higher one; off keeps only the highest.'),
    sub('levelRolesSyncOnDemotion', true, 'Remove rewards on XP loss', 'Members who lose XP also lose the reward roles above their new level.'),
  ]),
  customcommands: Object.freeze([
    sub('autoTriggersEnabled', true, 'Auto-responder triggers', 'Replies automatically when a message matches one of your /trigger keywords.'),
  ]),
  birthdays: Object.freeze([
    sub('birthdayRequireYear', false, 'Require birth year', 'Members must include the year when they save their birthday.'),
  ]),
  achievements: Object.freeze([
    sub('achievementUseEmbed', false, 'Announcement as an embed', 'Sends achievement unlock announcements as an embed; off sends plain text.'),
  ]),
  economy: Object.freeze([
    sub('economyImportGlobal', true, 'Copy the legacy global balance', "New accounts start once with the member's balance from the old global wallet."),
  ]),
  search: Object.freeze([
    sub('urbanNsfwOnly', true, '/urban only in age-restricted channels', 'Urban Dictionary lookups only work in age-restricted channels.'),
  ]),
});

/** Most changes one PATCH may carry. */
export const MAX_CHANGES = 60;

/** field -> default value, for every toggle the dashboard exposes. */
export const TOGGLE_DEFAULTS = Object.freeze(Object.fromEntries([
  ...MODULE_NAMES.map((key) => [MODULES[key].flag, MODULES[key].default === true]),
  ...Object.values(SUB_TOGGLES).flat().map((t) => [t.field, t.default]),
]));

function readOnlySet(values) {
  const set = new Set(values);
  const refuse = () => { throw new TypeError('TOGGLE_FIELDS is read-only'); };
  set.add = refuse;
  set.delete = refuse;
  set.clear = refuse;
  return Object.freeze(set);
}

/** Every GuildSettings field the dashboard may write: each module flag and each sub-toggle. Read-only. */
export const TOGGLE_FIELDS = readOnlySet(Object.keys(TOGGLE_DEFAULTS));

let catalogCache = null;

/**
 * The GET /api/catalog payload. Built once and deep-frozen.
 * @returns {{ categories: object[], modules: Record<string, object> }}
 */
export function buildCatalog() {
  if (catalogCache) return catalogCache;
  const modules = {};
  for (const category of CATEGORIES) {
    for (const key of category.modules) {
      const mod = MODULES[key];
      modules[key] = Object.freeze({
        key,
        label: mod.label,
        emoji: mod.emoji,
        flag: mod.flag,
        description: DESCRIPTIONS[key],
        configCommand: mod.configCommand,
        default: mod.default === true,
        subToggles: Object.freeze((SUB_TOGGLES[key] ?? []).map((t) => Object.freeze({ ...t }))),
      });
    }
  }
  catalogCache = Object.freeze({
    categories: Object.freeze(CATEGORIES.map((c) => Object.freeze({ ...c, modules: Object.freeze([...c.modules]) }))),
    modules: Object.freeze(modules),
  });
  return catalogCache;
}

/**
 * The toggle values of a GuildSettings row. A missing or non-boolean value reads as the catalog default.
 * @param {object|null|undefined} row
 * @returns {Record<string, boolean>} one entry per TOGGLE_FIELDS field
 */
export function pickToggleValues(row) {
  const values = {};
  for (const field of TOGGLE_FIELDS) {
    const value = row?.[field];
    values[field] = typeof value === 'boolean' ? value : TOGGLE_DEFAULTS[field];
  }
  return values;
}

const isPlainObject = (v) => {
  if (v === null || typeof v !== 'object' || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
};

const FORBIDDEN_KEYS = new Set(['__proto__', 'constructor', 'prototype']);

/**
 * Validates a PATCH body `{ changes: { [field]: boolean } }`: 1..MAX_CHANGES entries, every key a
 * TOGGLE_FIELDS field, every value strictly boolean, nothing else in the body.
 * @param {unknown} body parsed JSON
 * @returns {{ ok: true, changes: Record<string, boolean> } | { ok: false, message: string }}
 */
export function validateChanges(body) {
  if (!isPlainObject(body)) return { ok: false, message: 'The body must be a JSON object.' };
  const extra = Object.keys(body).filter((k) => k !== 'changes');
  if (extra.length > 0) return { ok: false, message: 'The body may only contain "changes".' };
  const input = body.changes;
  if (!isPlainObject(input)) return { ok: false, message: '"changes" must be an object of field: true/false.' };

  const keys = Object.keys(input);
  if (keys.length === 0) return { ok: false, message: '"changes" is empty.' };
  if (keys.length > MAX_CHANGES) return { ok: false, message: `At most ${MAX_CHANGES} changes per request.` };

  const changes = {};
  for (const key of keys) {
    if (FORBIDDEN_KEYS.has(key) || !TOGGLE_FIELDS.has(key)) {
      return { ok: false, message: `"${key.slice(0, 64)}" is not a setting the dashboard can change.` };
    }
    if (typeof input[key] !== 'boolean') return { ok: false, message: `"${key}" must be true or false.` };
    changes[key] = input[key];
  }
  return { ok: true, changes };
}
