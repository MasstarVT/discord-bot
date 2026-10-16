export const COLORS = {
  SUCCESS: 0x57f287,
  ERROR:   0xed4245,
  WARNING: 0xfee75c,
  INFO:    0x5865f2,
  PRIMARY: 0x2b2d31,
  NEUTRAL: 0x99aab5,
};

export const PERMISSION_LEVELS = {
  MEMBER:    0,
  MODERATOR: 1,
  ADMIN:     2,
  OWNER:     3,
};

// ── Numeric clamps ────────────────────────────────────────────────────────────
export const XP_MAX   = 1_000_000_000;  // UserLevel.xp clamp (Postgres int4)
export const COIN_MAX = 1_000_000_000;  // absolute economy clamp (GuildSettings.economyMaxBalance may be lower)

// ── Per-guild caps ────────────────────────────────────────────────────────────
// Packages MUST import these instead of hard-coding numbers (00-overview rule 19).
export const LIMITS = Object.freeze({
  SAVED_EMBEDS: 500, REMINDERS: 100, ACTIVE_GIVEAWAYS: 100, GIVEAWAY_WINNERS: 90,
  ACTIVE_POLLS: 50, POLL_OPTIONS: 20, STARBOARDS: 10, TICKET_PANELS: 25, TICKET_PANEL_OPTIONS: 25,
  TICKET_MANAGER_ROLES: 10, TEMPVOICE_HUBS: 10, STAT_CHANNELS: 20, ROLE_STAT_CHANNELS: 8,
  ACHIEVEMENTS: 50, ACHIEVEMENT_TIERS: 4, AUTOMATIONS: 50, AUTOMATION_ACTIONS: 10, AUTOMATION_CONDITIONS: 10,
  SHOP_ITEMS: 300, ITEM_COPIES_PER_USER: 200, CUSTOM_TAGS: 500, TAG_ACTIONS: 5, TAG_RESPONSES: 10,
  AUTO_TRIGGERS: 100, FEEDS: 30, ESCALATION_TIERS: 10, AUTOMOD_WORDS: 500, AUTOMOD_PATTERNS: 50,
  AUTOMOD_DOMAINS: 200, REACTION_ROLE_PANELS: 100, LEVEL_ROLES: 100, XP_BOOST_ROLES: 10,
  IMMUNE_ROLES: 25, MOD_ROLES: 25, AUTOROLES: 10,
  // Added by the architect review (package requests). Packages use them directly, with no `?? n` fallback.
  AUTOMOD_EXEMPTIONS: 50,      // automodConfig global and per-rule role/channel lists
  LOG_IGNORED_CHANNELS: 100,   // GuildSettings.logIgnoredChannelIds
  NOXP_ROLES: 100, NOXP_CHANNELS: 100,                 // GuildSettings.noXpRoleIds / noXpChannelIds
  TAG_ACCESS_ENTRIES: 25,      // each allow/deny role or channel list of a custom command
  TRIGGER_SCOPE_ENTRIES: 25,   // AutoTrigger.channelIds / exemptRoleIds
  TAG_PING_ROLES: 5,           // pingRoleIds per custom-command message action
  TRANSLATE_CHANNELS: 50,      // TranslateChannel rows per guild
  ECONOMY_GAME_CHANNELS: 25,   // GuildSettings.economyGameChannelIds
  ECONOMY_BOOSTS: 10,          // EconomyBoost rows per guild (MEE6 "up to 10")
  TIMEZONE_ROLES: 25,          // GuildSettings.timezoneRoles (/settings timezone-role)
});

// ── Module defaults ───────────────────────────────────────────────────────────
// Generated from build-plan.mjs (01-foundation §3.1). MODULES in modules.js reads these.
export const DEFAULT_MODULE_SETTINGS = Object.freeze({
  moderation: true,
  automod: false,
  logging: false,
  welcome: false,
  captcha: false,
  reactionroles: true,
  leveling: false,
  customcommands: false,
  automations: false,
  translation: false,
  invites: true,
  embeds: true,
  reminders: true,
  giveaways: true,
  polls: true,
  starboard: true,
  tickets: true,
  tempvoice: true,
  stats: true,
  birthdays: true,
  achievements: false,
  economy: true,
  games: true,
  feeds: true,
  search: true,
  liverole: false,
  counting: false,
});

export const XP_DEFAULTS = {
  rate:            15,
  min:             15,
  max:             25,
  cooldown:        60,
  levelUpChannel:  null,
};

// XP formula: 5 * (level^2) + 50 * level + 100
export const xpForLevel = (level) => 5 * (level ** 2) + 50 * level + 100;
