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

export const DEFAULT_MODULE_SETTINGS = {
  moderation:     false,
  logging:        false,
  welcome:        false,
  reactionRoles:  false,
  leveling:       false,
  customCommands: false,
  automod:        false,
  translation:    false,
};

export const XP_DEFAULTS = {
  rate:            15,
  cooldown:        60,
  levelUpChannel:  null,
};

// XP formula: 5 * (level^2) + 50 * level + 100
export const xpForLevel = (level) => 5 * (level ** 2) + 50 * level + 100;
