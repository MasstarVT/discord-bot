// The `triggers` collection (Custom Commands → auto-responders): the AutoTrigger rows behind /trigger, which answer
// messages that contain, equal, start or end with a keyword, or match a regular expression.
// Pure: nothing is read or connected at import time (safeRegex only validates patterns).
import { LIMITS } from '../../config/constants.js';
import { validateRegex } from '../../utils/safeRegex.js';

/** tagService.TRIGGER_PATTERN_MAX and TRIGGER_COOLDOWN_MAX, and the /trigger response option's length. */
export const TRIGGER_LIMITS = Object.freeze({ pattern: 500, response: 2000, cooldown: 86_400 });

export const MATCH_TYPE_OPTIONS = Object.freeze([
  { value: 'CONTAINS', label: 'Contains' },
  { value: 'EXACT', label: 'Is exactly' },
  { value: 'STARTS_WITH', label: 'Starts with' },
  { value: 'ENDS_WITH', label: 'Ends with' },
  { value: 'REGEX', label: 'Regular expression' },
]);

export const collection = Object.freeze({
  key: 'triggers',
  module: 'customcommands',
  label: 'Auto-responders',
  singular: 'auto-responder',
  description: 'Automatic replies to messages that contain a keyword or match a pattern. They answer while the Custom '
    + 'Commands module and its auto-responder switch are on.',
  limit: LIMITS.AUTO_TRIGGERS,
  titleField: 'trigger',
  subtitleField: 'response',
  fields: Object.freeze([
    {
      field: 'trigger',
      type: 'text',
      label: 'Keyword or pattern',
      description: 'The word, phrase or regular expression to watch for.',
      maxLength: TRIGGER_LIMITS.pattern,
      minLength: 1,
      required: true,
    },
    { field: 'matchType', type: 'enum', label: 'Match', options: MATCH_TYPE_OPTIONS, default: 'CONTAINS' },
    { field: 'caseSensitive', type: 'boolean', label: 'Match upper/lower case exactly', default: false },
    {
      field: 'response',
      type: 'text',
      label: 'Response',
      description: 'The reply. Variables such as {user} and {channel} work.',
      multiline: true,
      maxLength: TRIGGER_LIMITS.response,
      minLength: 1,
      placeholders: [],
      required: true,
    },
    {
      field: 'cooldown',
      type: 'int',
      label: 'Cooldown per member',
      description: 'How long one member waits before it answers them again (0 = no cooldown).',
      min: 0,
      max: TRIGGER_LIMITS.cooldown,
      unit: 'seconds',
      default: 0,
    },
    {
      field: 'deleteMessage',
      type: 'boolean',
      label: 'Delete the matching message',
      description: 'The bot needs Manage Messages for this.',
      default: false,
    },
    {
      field: 'channelIds',
      type: 'channels',
      label: 'Only in these channels',
      description: 'Empty: every channel. Threads count as their parent channel.',
      channelTypes: ['text', 'announcement', 'forum', 'voice'],
      maxItems: LIMITS.TRIGGER_SCOPE_ENTRIES,
      default: [],
    },
    {
      field: 'exemptRoleIds',
      type: 'roles',
      label: 'Ignore members with these roles',
      maxItems: LIMITS.TRIGGER_SCOPE_ENTRIES,
      default: [],
    },
    { field: 'enabled', type: 'boolean', label: 'Enabled', description: 'Off: paused, the settings are kept.', default: true },
  ]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([
    { field: 'uses', type: 'int', label: 'Times answered', min: 0, max: 2_147_483_647, default: 0 },
    { field: 'createdAt', type: 'datetime', label: 'Created', nullable: true, default: null },
    { field: 'updatedAt', type: 'datetime', label: 'Last edited', nullable: true, default: null },
  ]),
  actions: Object.freeze([]),
  check: checkTrigger,
});

/**
 * The pattern rule of /trigger add and edit (validateTriggerPattern): a REGEX pattern must pass safeRegex
 * (no nested repetition or backreferences, at most 200 characters, valid syntax).
 * @returns {null|{ path: string, message: string }}
 */
export function checkTrigger(data, { mode, current } = {}) {
  const t = mode === 'update' && current ? { ...current, ...data } : data;
  if (!['trigger', 'matchType', 'caseSensitive'].some((k) => Object.hasOwn(data, k))) return null;
  if (typeof t.trigger !== 'string' || t.trigger.trim() === '') return { path: 'trigger', message: 'The pattern is empty.' };
  if (t.matchType === 'REGEX') {
    const v = validateRegex(t.trigger, t.caseSensitive ? '' : 'i');
    if (!v.ok) return { path: 'trigger', message: v.reason };
  }
  return null;
}

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const list = (v) => (Array.isArray(v) ? v : []);

/** An AutoTrigger row as a collection item. */
export function triggerItem(row) {
  return {
    id: row.id,
    trigger: row.trigger,
    matchType: row.matchType,
    caseSensitive: row.caseSensitive === true,
    response: row.response,
    cooldown: Number.isInteger(row.cooldown) ? row.cooldown : 0,
    deleteMessage: row.deleteMessage === true,
    channelIds: list(row.channelIds),
    exemptRoleIds: list(row.exemptRoleIds),
    enabled: row.enabled !== false,
    uses: Number.isInteger(row.uses) ? row.uses : 0,
    createdAt: iso(row.createdAt),
    updatedAt: iso(row.updatedAt),
  };
}
