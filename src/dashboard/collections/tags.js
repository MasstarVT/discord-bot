// The `tags` collection (Custom Commands): the CustomTag rows behind /tag and the MEE6-style `!name` commands, with
// every option the slash command sets (actions with random replies, access lists, cooldown, delete usage).
// Also the pure conversion between the stored action list (tagService.normalizeActions) and the form's shape.
// Pure: nothing is read or connected at import time.
import { LIMITS } from '../../config/constants.js';

/** Limits of the custom-command writes (tagService: PLAIN_MAX, EMBED_TEXT_MAX, DESCRIPTION_MAX, COOLDOWN_MAX_SECONDS). */
export const TAG_LIMITS = Object.freeze({ plain: 2000, embed: 4000, description: 100, cooldownSeconds: 31_536_000, name: 32 });

const MESSAGE_TYPES = Object.freeze(['reply', 'dm', 'send']);
const ROLE_TYPES = Object.freeze(['addRole', 'removeRole', 'toggleRole']);

/** Argument variables a custom command's replies support (besides the built-in ones). */
export const ARGUMENT_PLACEHOLDERS = Object.freeze(['{1}', '{1:default}', '{2...}', '{...3}', '{2...4}', '{...}']);

/** Channel kinds the access lists accept (the /tag permissions channel option, categories included). */
const ACCESS_CHANNEL_KINDS = Object.freeze(['text', 'announcement', 'forum', 'voice', 'category']);

const ACTION_FIELDS = Object.freeze([
  {
    field: 'type',
    type: 'enum',
    label: 'Action',
    options: [
      { value: 'reply', label: 'Reply in the channel' },
      { value: 'dm', label: 'Reply by DM' },
      { value: 'send', label: 'Send to a channel' },
      { value: 'addRole', label: 'Give a role' },
      { value: 'removeRole', label: 'Remove a role' },
      { value: 'toggleRole', label: 'Toggle a role' },
    ],
    default: 'reply',
  },
  {
    field: 'responses',
    type: 'objectList',
    label: 'Replies',
    description: 'Reply, DM and send actions: one of these is picked at random each time.',
    maxItems: LIMITS.TAG_RESPONSES,
    itemFields: [
      {
        field: 'text',
        type: 'text',
        label: 'Reply',
        description: 'Up to 2,000 characters, or 4,000 when sent as an embed.',
        multiline: true,
        maxLength: TAG_LIMITS.embed,
        minLength: 1,
        placeholders: [...ARGUMENT_PLACEHOLDERS],
        default: 'Hello {user}!',
      },
    ],
    default: [],
  },
  { field: 'embed', type: 'boolean', label: 'Send as an embed', description: 'Reply, DM and send actions only.', default: false },
  {
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: '"Send to a channel" only: where the message goes.',
    channelTypes: ['text', 'announcement'],
    nullable: true,
    default: null,
  },
  {
    field: 'roleId',
    type: 'role',
    label: 'Role',
    description: 'Role actions only. It must be below the bot\'s highest role and carry no moderator permissions.',
    assignable: true,
    nullable: true,
    default: null,
  },
  {
    field: 'pingRoleIds',
    type: 'roles',
    label: 'Roles the message may ping',
    description: 'Reply and send actions: role mentions in the text only ping these roles. DMs never ping.',
    maxItems: LIMITS.TAG_PING_ROLES,
    default: [],
  },
]);

const accessRoles = (field, label, description) => ({
  field, type: 'roles', label, description, maxItems: LIMITS.TAG_ACCESS_ENTRIES, default: [],
});
const accessChannels = (field, label, description) => ({
  field, type: 'channels', label, description, channelTypes: [...ACCESS_CHANNEL_KINDS], maxItems: LIMITS.TAG_ACCESS_ENTRIES, default: [],
});

const asList = (v) => (Array.isArray(v) ? v : []);

export const collection = Object.freeze({
  key: 'tags',
  module: 'customcommands',
  label: 'Custom commands',
  singular: 'custom command',
  description: 'Commands members run with the server prefix (!name) or /tag use: replies with random variations, '
    + 'DMs, messages to other channels and roles to give or take, with access rules and cooldowns.',
  limit: LIMITS.CUSTOM_TAGS,
  titleField: 'name',
  subtitleField: 'description',
  fields: Object.freeze([
    {
      field: 'name',
      type: 'text',
      label: 'Name',
      description: 'Members type it after the prefix, e.g. rules for !rules. Lowercase letters, numbers, - and _. It cannot be changed later.',
      maxLength: TAG_LIMITS.name,
      minLength: 1,
      pattern: '[a-z0-9_-]{1,32}',
      patternMessage: 'Use lowercase letters, numbers, - and _ only (up to 32 characters), e.g. server-rules.',
      required: true,
    },
    {
      field: 'description',
      type: 'text',
      label: 'Description',
      description: 'Shown in /help and /tag list.',
      maxLength: TAG_LIMITS.description,
      nullable: true,
      default: null,
    },
    { field: 'enabled', type: 'boolean', label: 'Enabled', description: 'Off: the command does not answer.', default: true },
    {
      field: 'actions',
      type: 'objectList',
      label: 'Actions',
      description: `What the command does, in order (up to ${LIMITS.TAG_ACTIONS}). Replies support variables and arguments such as {1}.`,
      itemFields: ACTION_FIELDS,
      minItems: 1,
      maxItems: LIMITS.TAG_ACTIONS,
      default: [{ type: 'reply', responses: [{ text: 'Hello {user}!' }], embed: false, channelId: null, roleId: null, pingRoleIds: [] }],
    },
    accessRoles('allowRoleIds', 'Allowed roles', 'Only members with one of these roles may use it. Empty: everyone.'),
    accessRoles('denyRoleIds', 'Denied roles', 'Members with one of these roles may not use it.'),
    accessChannels('allowChannelIds', 'Allowed channels', 'Only works in these channels or categories. Empty: everywhere.'),
    accessChannels('denyChannelIds', 'Denied channels', 'Never works in these channels or categories.'),
    {
      field: 'cooldownScope',
      type: 'enum',
      label: 'Cooldown',
      options: [
        { value: 'NONE', label: 'No cooldown' },
        { value: 'SERVER', label: 'Server-wide' },
        { value: 'USER', label: 'Per member' },
      ],
      default: 'NONE',
    },
    {
      field: 'cooldownSeconds',
      type: 'int',
      label: 'Cooldown length',
      description: 'How long before it can be used again (up to 365 days).',
      min: 0,
      max: TAG_LIMITS.cooldownSeconds,
      unit: 'seconds',
      default: 0,
    },
    {
      field: 'deleteInvocation',
      type: 'boolean',
      label: 'Delete the !name message',
      description: 'Deletes the member\'s message after the command ran (prefix use only; the bot needs Manage Messages).',
      default: false,
    },
  ]),
  createOnly: Object.freeze(['name']),
  readOnly: Object.freeze([
    { field: 'uses', type: 'int', label: 'Uses', min: 0, max: 2_147_483_647, default: 0 },
    { field: 'createdAt', type: 'datetime', label: 'Created', nullable: true, default: null },
    { field: 'updatedAt', type: 'datetime', label: 'Last edited', nullable: true, default: null },
  ]),
  actions: Object.freeze([]),
  check: checkTag,
});

/**
 * Cross-field rules of a custom command (the /tag action add and cooldown checks). On update `current` (the stored
 * item) fills in what the request leaves out.
 * @returns {null|{ path: string, message: string }}
 */
export function checkTag(data, { mode, current } = {}) {
  const t = mode === 'update' && current ? { ...current, ...data } : data;
  const actions = asList(t.actions);
  for (const [i, a] of actions.entries()) {
    const at = `actions.${i}`;
    if (MESSAGE_TYPES.includes(a.type)) {
      const replies = asList(a.responses);
      if (!replies.length) return { path: `${at}.responses`, message: 'Add at least one reply.' };
      const max = a.embed ? TAG_LIMITS.embed : TAG_LIMITS.plain;
      for (const [k, r] of replies.entries()) {
        if (String(r?.text ?? '').length > max) {
          return {
            path: `${at}.responses.${k}.text`,
            message: a.embed
              ? `Embed replies can be at most 4,000 characters (this one is ${r.text.length}).`
              : `Plain replies can be at most 2,000 characters (this one is ${r.text.length}). Turn on "Send as an embed" for up to 4,000.`,
          };
        }
      }
      if (a.type === 'send' && !a.channelId) return { path: `${at}.channelId`, message: 'Pick the channel to send to.' };
      if (a.type === 'dm' && asList(a.pingRoleIds).length) return { path: `${at}.pingRoleIds`, message: 'DMs never ping roles.' };
    } else if (ROLE_TYPES.includes(a.type)) {
      if (!a.roleId) return { path: `${at}.roleId`, message: 'Pick the role.' };
    }
  }
  for (const [allow, deny, what] of [['allowRoleIds', 'denyRoleIds', 'role'], ['allowChannelIds', 'denyChannelIds', 'channel']]) {
    const allowed = new Set(asList(t[allow]));
    const both = asList(t[deny]).findIndex((id) => allowed.has(id));
    if (both >= 0) return { path: `${deny}.${both}`, message: `A ${what} cannot be both allowed and denied.` };
  }
  if (t.cooldownScope && t.cooldownScope !== 'NONE' && !(t.cooldownSeconds >= 1)) {
    return { path: 'cooldownSeconds', message: 'Set a cooldown of at least 1 second, or pick "No cooldown".' };
  }
  return null;
}

// ── Stored actions <-> form actions ──────────────────────────────────────────

/**
 * One stored action (tagService.normalizeActions shape) in the form's shape.
 * @param {object} a { type: 'reply'|'send'|'addRole'|..., dm?, embed?, responses?, channelId?, roleId?, pingRoleIds? }
 * @returns {{ type: string, responses: { text: string }[], embed: boolean, channelId: string|null, roleId: string|null, pingRoleIds: string[] }}
 */
export function actionToForm(a) {
  const isRole = ROLE_TYPES.includes(a?.type);
  return {
    type: a?.type === 'reply' && a.dm ? 'dm' : a?.type,
    responses: isRole ? [] : asList(a?.responses).map((text) => ({ text: String(text) })),
    embed: !isRole && a?.embed === true,
    channelId: a?.type === 'send' ? a.channelId ?? null : null,
    roleId: isRole ? a.roleId ?? null : null,
    pingRoleIds: isRole || (a?.type === 'reply' && a.dm) ? [] : asList(a?.pingRoleIds),
  };
}

/**
 * One form action (validated) in the stored shape normalizeActions keeps. Options that do not belong to the action
 * type (a role on a reply, replies on a role action) are dropped.
 * @returns {object}
 */
export function actionToStored(f) {
  if (ROLE_TYPES.includes(f.type)) return { type: f.type, roleId: f.roleId };
  const responses = asList(f.responses).map((r) => r.text);
  const embed = f.embed === true;
  const pings = [...new Set(asList(f.pingRoleIds))].slice(0, LIMITS.TAG_PING_ROLES);
  if (f.type === 'dm') return { type: 'reply', dm: true, embed, responses };
  const out = f.type === 'send'
    ? { type: 'send', channelId: f.channelId, embed, responses }
    : { type: 'reply', dm: false, embed, responses };
  if (pings.length) out.pingRoleIds = pings;
  return out;
}

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);

/**
 * A CustomTag row as a collection item.
 * @param {object} row
 * @param {object[]} effectiveActions tagService.getEffectiveActions(row) (legacy rows: their content as one reply)
 */
export function tagItem(row, effectiveActions) {
  return {
    id: row.id,
    name: row.name,
    description: row.description ?? null,
    enabled: row.enabled !== false,
    actions: asList(effectiveActions).map(actionToForm),
    allowRoleIds: asList(row.allowRoleIds),
    denyRoleIds: asList(row.denyRoleIds),
    allowChannelIds: asList(row.allowChannelIds),
    denyChannelIds: asList(row.denyChannelIds),
    cooldownScope: ['NONE', 'SERVER', 'USER'].includes(row.cooldownScope) ? row.cooldownScope : 'NONE',
    cooldownSeconds: Number.isInteger(row.cooldownSeconds) ? Math.max(0, row.cooldownSeconds) : 0,
    deleteInvocation: row.deleteInvocation === true,
    uses: Number.isInteger(row.uses) ? row.uses : 0,
    createdAt: iso(row.createdAt),
    updatedAt: iso(row.updatedAt),
  };
}
