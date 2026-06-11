// Maps module names to their GuildSettings toggle field.
// Used by permission checks and the /module enable|disable command.
export const MODULE_FLAGS = {
  moderation:     'modEnabled',
  logging:        'loggingEnabled',
  welcome:        'welcomeEnabled',
  reactionroles:  'reactionRolesEnabled',
  leveling:       'levelingEnabled',
  customcommands: 'customCommandsEnabled',
  automod:        'automodEnabled',
  translation:    'translationEnabled',
};

export const MODULE_NAMES = Object.keys(MODULE_FLAGS);
