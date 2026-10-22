// Module registry: one entry per toggleable module, keyed by module name.
// MODULES (flag, label, emoji, default) is the single source for MODULE_FLAGS and
// DEFAULT_MODULE_SETTINGS; the router, the hook runner, /module and /help all read it.
// Pure: imports only constants.js.
import { DEFAULT_MODULE_SETTINGS } from './constants.js';

export const MODULES = Object.freeze({
  moderation: Object.freeze({ flag: "modEnabled", label: "Moderation", emoji: "🔨", mee6: "Moderator", configCommand: "/modconfig", default: DEFAULT_MODULE_SETTINGS.moderation }),
  automod: Object.freeze({ flag: "automodEnabled", label: "Auto-Moderation", emoji: "🤖", mee6: "Moderator › Auto-moderation", configCommand: "/automod", default: DEFAULT_MODULE_SETTINGS.automod }),
  logging: Object.freeze({ flag: "loggingEnabled", label: "Audit Logs", emoji: "📜", mee6: "Moderator › Audit logging", configCommand: "/logging", default: DEFAULT_MODULE_SETTINGS.logging }),
  welcome: Object.freeze({ flag: "welcomeEnabled", label: "Welcome & Goodbye", emoji: "👋", mee6: "Welcome & Goodbye", configCommand: "/welcome", default: DEFAULT_MODULE_SETTINGS.welcome }),
  captcha: Object.freeze({ flag: "captchaEnabled", label: "Verification", emoji: "🛡️", mee6: "Welcome › Captcha verification", configCommand: "/captcha", default: DEFAULT_MODULE_SETTINGS.captcha }),
  reactionroles: Object.freeze({ flag: "reactionRolesEnabled", label: "Reaction Roles", emoji: "🎭", mee6: "Reaction Roles", configCommand: "/reactionrole", default: DEFAULT_MODULE_SETTINGS.reactionroles }),
  leveling: Object.freeze({ flag: "xpEnabled", label: "Levels", emoji: "⭐", mee6: "Levels", configCommand: "/levelconfig", default: DEFAULT_MODULE_SETTINGS.leveling }),
  customcommands: Object.freeze({ flag: "customCommandsEnabled", label: "Custom Commands", emoji: "🏷️", mee6: "Custom Commands", configCommand: "/tag", default: DEFAULT_MODULE_SETTINGS.customcommands }),
  automations: Object.freeze({ flag: "automationsEnabled", label: "Automations", emoji: "⚙️", mee6: "Automations", configCommand: "/automation", default: DEFAULT_MODULE_SETTINGS.automations }),
  translation: Object.freeze({ flag: "translationEnabled", label: "Auto-Translate", emoji: "🌐", mee6: "(ours, beyond MEE6)", configCommand: "/autotranslate", default: DEFAULT_MODULE_SETTINGS.translation }),
  invites: Object.freeze({ flag: "invitesEnabled", label: "Invite Tracker", emoji: "📨", mee6: "Invite Tracker", configCommand: "/invites", default: DEFAULT_MODULE_SETTINGS.invites }),
  embeds: Object.freeze({ flag: "embedsEnabled", label: "Embed Messages", emoji: "🧱", mee6: "Embed Messages", configCommand: "/embed", default: DEFAULT_MODULE_SETTINGS.embeds }),
  reminders: Object.freeze({ flag: "remindersEnabled", label: "Reminders", emoji: "⏰", mee6: "Reminder (Reminders/Timers)", configCommand: "/reminder", default: DEFAULT_MODULE_SETTINGS.reminders }),
  giveaways: Object.freeze({ flag: "giveawaysEnabled", label: "Giveaways", emoji: "🎉", mee6: "Giveaways", configCommand: "/giveaway", default: DEFAULT_MODULE_SETTINGS.giveaways }),
  polls: Object.freeze({ flag: "pollsEnabled", label: "Polls", emoji: "📊", mee6: "Polls", configCommand: "/poll", default: DEFAULT_MODULE_SETTINGS.polls }),
  starboard: Object.freeze({ flag: "starboardEnabled", label: "Starboards", emoji: "🌟", mee6: "Starboards", configCommand: "/starboard", default: DEFAULT_MODULE_SETTINGS.starboard }),
  tickets: Object.freeze({ flag: "ticketsEnabled", label: "Ticketing", emoji: "🎫", mee6: "Ticketing", configCommand: "/ticket-panel", default: DEFAULT_MODULE_SETTINGS.tickets }),
  tempvoice: Object.freeze({ flag: "tempVoiceEnabled", label: "Temporary Channels", emoji: "🔊", mee6: "Temporary Channels", configCommand: "/voice-hub", default: DEFAULT_MODULE_SETTINGS.tempvoice }),
  stats: Object.freeze({ flag: "statsEnabled", label: "Statistics Channels", emoji: "📈", mee6: "Statistics Channels", configCommand: "/stats", default: DEFAULT_MODULE_SETTINGS.stats }),
  birthdays: Object.freeze({ flag: "birthdaysEnabled", label: "Birthdays", emoji: "🎂", mee6: "Birthdays", configCommand: "/birthday", default: DEFAULT_MODULE_SETTINGS.birthdays }),
  achievements: Object.freeze({ flag: "achievementsEnabled", label: "Achievements", emoji: "🏆", mee6: "Achievements", configCommand: "/achievement-config", default: DEFAULT_MODULE_SETTINGS.achievements }),
  economy: Object.freeze({ flag: "economyEnabled", label: "Economy", emoji: "🪙", mee6: "Economy", configCommand: "/economy-admin", default: DEFAULT_MODULE_SETTINGS.economy }),
  games: Object.freeze({ flag: "gamesEnabled", label: "Games", emoji: "🎮", mee6: "Economy › games (+ ours)", configCommand: "/games", default: DEFAULT_MODULE_SETTINGS.games }),
  feeds: Object.freeze({ flag: "feedsEnabled", label: "Social Feeds", emoji: "📰", mee6: "RSS / YouTube / Reddit / Bluesky / Podcast alerts", configCommand: "/feed", default: DEFAULT_MODULE_SETTINGS.feeds }),
  search: Object.freeze({ flag: "searchEnabled", label: "Search Anything", emoji: "🔎", mee6: "Search Anything", configCommand: "/search", default: DEFAULT_MODULE_SETTINGS.search }),
  liverole: Object.freeze({ flag: "liveRoleEnabled", label: "Live Role", emoji: "🔴", mee6: "Twitch Alerts › live-streaming role", configCommand: "/liverole", default: DEFAULT_MODULE_SETTINGS.liverole }),
  counting: Object.freeze({ flag: "countingEnabled", label: "Counting", emoji: "🔢", mee6: "(ours, beyond MEE6)", configCommand: "/counting", default: DEFAULT_MODULE_SETTINGS.counting }),
  autotime: Object.freeze({ flag: "autoTimeEnabled", label: "Auto Timestamps", emoji: "🕒", mee6: "(ours, beyond MEE6)", configCommand: "/autotime", default: DEFAULT_MODULE_SETTINGS.autotime }),
  suggestions: Object.freeze({ flag: "suggestionsEnabled", label: "Suggestions", emoji: "💡", mee6: "(ours, beyond MEE6)", configCommand: "/suggestion", default: DEFAULT_MODULE_SETTINGS.suggestions }),
  modmail: Object.freeze({ flag: "modmailEnabled", label: "Modmail", emoji: "📬", mee6: "(ours, beyond MEE6)", configCommand: "/modmail", default: DEFAULT_MODULE_SETTINGS.modmail }),
  applications: Object.freeze({ flag: "applicationsEnabled", label: "Applications", emoji: "📝", mee6: "(ours, beyond MEE6)", configCommand: "/application", default: DEFAULT_MODULE_SETTINGS.applications }),
  sticky: Object.freeze({ flag: "stickyEnabled", label: "Sticky Messages", emoji: "📌", mee6: "(ours, beyond MEE6)", configCommand: "/sticky", default: DEFAULT_MODULE_SETTINGS.sticky }),
  afk: Object.freeze({ flag: "afkEnabled", label: "AFK", emoji: "💤", mee6: "(ours, beyond MEE6)", configCommand: "/afk", default: DEFAULT_MODULE_SETTINGS.afk }),
  antiraid: Object.freeze({ flag: "antiRaidEnabled", label: "Anti-Raid", emoji: "🚨", mee6: "(ours, beyond MEE6)", configCommand: "/antiraid", default: DEFAULT_MODULE_SETTINGS.antiraid }),
  events: Object.freeze({ flag: "eventsEnabled", label: "Events", emoji: "📅", mee6: "(ours, beyond MEE6)", configCommand: "/event", default: DEFAULT_MODULE_SETTINGS.events }),
  rolepersist: Object.freeze({ flag: "rolePersistEnabled", label: "Role Persistence", emoji: "🔁", mee6: "(ours, beyond MEE6)", configCommand: "/rolepersist", default: DEFAULT_MODULE_SETTINGS.rolepersist }),
  boosts: Object.freeze({ flag: "boostsEnabled", label: "Server Boosts", emoji: "💎", mee6: "(ours, beyond MEE6)", configCommand: "/boost", default: DEFAULT_MODULE_SETTINGS.boosts }),
  backups: Object.freeze({ flag: "backupsEnabled", label: "Server Backups", emoji: "💾", mee6: "(ours, beyond MEE6)", configCommand: "/backup", default: DEFAULT_MODULE_SETTINGS.backups }),
  confessions: Object.freeze({ flag: "confessionsEnabled", label: "Confessions", emoji: "🤫", mee6: "(ours, beyond MEE6)", configCommand: "/confession", default: DEFAULT_MODULE_SETTINGS.confessions }),
  autothreads: Object.freeze({ flag: "autoThreadsEnabled", label: "Auto Threads", emoji: "🧵", mee6: "(ours, beyond MEE6)", configCommand: "/autothread", default: DEFAULT_MODULE_SETTINGS.autothreads }),
  voiceroles: Object.freeze({ flag: "voiceRolesEnabled", label: "Voice Roles", emoji: "🎙️", mee6: "(ours, beyond MEE6)", configCommand: "/voicerole", default: DEFAULT_MODULE_SETTINGS.voiceroles }),
  bumps: Object.freeze({ flag: "bumpsEnabled", label: "Bump Reminders", emoji: "📣", mee6: "(ours, beyond MEE6)", configCommand: "/bumps", default: DEFAULT_MODULE_SETTINGS.bumps }),
});

export const MODULE_NAMES = Object.freeze(Object.keys(MODULES));

// Legacy export name: module name -> GuildSettings toggle field.
// `leveling` maps to xpEnabled; levelingEnabled stays deprecated and unread (R256/R257).
export const MODULE_FLAGS = Object.freeze(Object.fromEntries(MODULE_NAMES.map((k) => [k, MODULES[k].flag])));

/** @returns {object|null} the registry entry for a module key, or null */
export function getModule(key) {
  return typeof key === 'string' && Object.hasOwn(MODULES, key) ? MODULES[key] : null;
}

/** @returns {boolean} true when `key` names a registered module */
export function isModuleKey(key) {
  return getModule(key) !== null;
}

/**
 * Pure toggle check, no logging.
 * key == null -> true (core, ungated); unknown key -> false; settings == null -> false;
 * otherwise the module's GuildSettings flag must be exactly true.
 * @param {object|null} settings GuildSettings row
 * @param {string|null|undefined} key MODULES key
 */
export function isModuleEnabled(settings, key) {
  if (key == null) return true;
  const mod = getModule(key);
  if (!mod) return false;
  if (settings == null) return false;
  return settings[mod.flag] === true;
}
