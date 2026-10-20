// What the dashboard shows and may change: module categories, one-line descriptions, every module's settings groups
// (src/dashboard/fields.js), its record lists (src/dashboard/collections/index.js) and the placeholder tokens.
// Pure: imports only registries.
import { MODULES } from '../config/modules.js';
import { PLACEHOLDERS } from '../utils/placeholderParser.js';
import { GENERAL_GROUPS, MODULE_GROUPS, SETTINGS_FIELDS } from './fields.js';
import { COLLECTION_LIST, publicCollection } from './collections/index.js';
import { publicDescriptor } from './schema.js';

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
    modules: Object.freeze(['leveling', 'achievements', 'economy', 'games', 'counting']),
  }),
  Object.freeze({
    id: 'tools',
    label: 'Content & Utilities',
    description: 'Custom commands, automations, scheduled messages, feeds and other handy tools.',
    modules: Object.freeze(['customcommands', 'automations', 'embeds', 'reminders', 'feeds', 'translation', 'search', 'liverole', 'autotime']),
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
  liverole: 'Gives members a role while they stream. Switching it off here leaves it on current streamers; /liverole disable in Discord removes it too.',
  counting: 'A channel where members count up from 1 in turns; a wrong number starts the count again.',
  autotime: 'Answers times typed in chat, like "fri 5pm", with a timestamp everyone sees in their own timezone.',
});

/**
 * A two-letter code per module (a capital and a small letter, unique), shown on its tile in the dashboard and on the
 * landing page's module table.
 */
export const CODES = Object.freeze({
  moderation: 'Md',
  automod: 'Am',
  logging: 'Lg',
  captcha: 'Cp',
  welcome: 'Wl',
  reactionroles: 'Rr',
  invites: 'Iv',
  tickets: 'Tk',
  tempvoice: 'Tv',
  stats: 'St',
  birthdays: 'Bd',
  polls: 'Pl',
  giveaways: 'Gw',
  starboard: 'Sb',
  leveling: 'Lv',
  achievements: 'Ac',
  economy: 'Ec',
  games: 'Gm',
  counting: 'Ct',
  customcommands: 'Cc',
  automations: 'Au',
  embeds: 'Em',
  reminders: 'Rm',
  feeds: 'Fd',
  translation: 'Tr',
  search: 'Sr',
  liverole: 'Lr',
  autotime: 'Ts',
});

/** The boolean sub-settings of each module (their descriptors live in fields.js). */
const SUB_TOGGLE_FIELDS = Object.freeze({
  moderation: ['modDmTargets'],
  welcome: ['welcomeMessageEnabled', 'welcomeUseEmbed', 'welcomeDmEnabled', 'welcomeDmCard', 'goodbyeEnabled', 'goodbyeUseEmbed'],
  leveling: ['voiceXpEnabled', 'levelUpUseEmbed', 'levelRolesStack', 'levelRolesSyncOnDemotion'],
  customcommands: ['autoTriggersEnabled'],
  birthdays: ['birthdayRequireYear'],
  achievements: ['achievementUseEmbed'],
  economy: ['economyImportGlobal'],
  search: ['urbanNsfwOnly'],
});

const sub = (field) => {
  const d = SETTINGS_FIELDS[field];
  return Object.freeze({ field, label: d.label, description: d.description, default: d.default });
};

/**
 * Boolean GuildSettings columns shown as switches inside each module (the `subToggles` of the catalog; they are also
 * fields of the module's groups). Never exposed: levelingEnabled (deprecated), captchaLockdownPrior (bot-managed state).
 */
export const SUB_TOGGLES = Object.freeze(Object.fromEntries(Object.entries(SUB_TOGGLE_FIELDS)
  .map(([key, fields]) => [key, Object.freeze(fields.map(sub))])));

const GENERAL_DESCRIPTION = 'Time zone, embed colour and the custom command prefix: settings every module uses.';

/** The group of a placeholder token for the browser's token picker. */
const placeholderGroup = (token) => {
  const ns = /^\{(user|server|channel)\b/.exec(token)?.[1];
  return ns ?? 'other';
};

const publicGroup = (g) => ({
  id: g.id,
  title: g.title,
  ...(g.description ? { description: g.description } : {}),
  fields: g.fields.map(publicDescriptor),
});

let catalogCache = null;

/**
 * The GET /api/catalog payload. Built once; JSON-safe and deep-frozen.
 * @returns {{ categories: object[], modules: Record<string, object>, general: object, collections: Record<string, object>,
 *   placeholders: { token: string, description: string, group: string }[] }}
 */
export function buildCatalog() {
  if (catalogCache) return catalogCache;
  const modules = {};
  for (const category of CATEGORIES) {
    for (const key of category.modules) {
      const mod = MODULES[key];
      modules[key] = {
        key,
        code: CODES[key],
        label: mod.label,
        emoji: mod.emoji,
        flag: mod.flag,
        description: DESCRIPTIONS[key],
        configCommand: mod.configCommand,
        default: mod.default === true,
        subToggles: (SUB_TOGGLES[key] ?? []).map((t) => ({ ...t })),
        groups: MODULE_GROUPS[key].map(publicGroup),
        collections: COLLECTION_LIST.filter((c) => c.module === key).map((c) => c.key),
      };
    }
  }
  const catalog = {
    categories: CATEGORIES.map((c) => ({ ...c, modules: [...c.modules] })),
    modules,
    general: { title: 'General', description: GENERAL_DESCRIPTION, groups: GENERAL_GROUPS.map(publicGroup) },
    collections: Object.fromEntries(COLLECTION_LIST.map((c) => [c.key, publicCollection(c)])),
    placeholders: Object.entries(PLACEHOLDERS).map(([token, description]) => ({ token, description, group: placeholderGroup(token) })),
  };
  // Served as is: a JSON round trip drops anything JSON cannot carry, so the payload equals what the browser parses.
  catalogCache = deepFreeze(JSON.parse(JSON.stringify(catalog)));
  return catalogCache;
}

function deepFreeze(v) {
  if (v && typeof v === 'object') {
    for (const child of Object.values(v)) deepFreeze(child);
    Object.freeze(v);
  }
  return v;
}
