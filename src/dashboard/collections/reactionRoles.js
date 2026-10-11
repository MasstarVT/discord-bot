// The `reactionRoles` collection (Reaction Roles): role panels members use to give themselves roles with buttons, a dropdown
// or emoji reactions (ReactionRoleGroup rows with their ReactionRoleItem rows as the `items` list), like /reactionrole.
// Pure: constants and helpers only. The limits that live in reactionRoleService.js (it opens Redis and Prisma at import) are
// copied here, and test/dashboard-panels.test.js fails when they drift apart.
import { LIMITS } from '../../config/constants.js';
import { clampText } from '../../utils/text.js';
import { RecordError } from './errors.js';
import { POSTABLE_CHANNEL_KINDS, messageUrl } from './embeds.js';

// ── Constants shared with the service and the tasks ──────────────────────────

export const RR_TYPES = Object.freeze(['NORMAL', 'UNIQUE', 'VERIFY', 'REVERSE']);
export const RR_LAYOUTS = Object.freeze(['BUTTONS', 'SELECT', 'REACTIONS']);
/** Most roles one panel can hold, per layout (a message has at most 20 reactions of the bot). */
export const MAX_ITEMS = Object.freeze({ BUTTONS: 25, SELECT: 25, REACTIONS: 20 });
export const TYPE_LABELS = Object.freeze({ NORMAL: 'Normal', UNIQUE: 'Unique', VERIFY: 'Verify', REVERSE: 'Reverse' });
export const LAYOUT_LABELS = Object.freeze({ BUTTONS: 'Buttons', SELECT: 'Dropdown', REACTIONS: 'Emoji reactions' });
export const DEFAULT_TITLE = 'Reaction Roles';
export const MAX_MAX_ROLES = 25;
/** What `messageId` holds while a panel has not been posted yet (the column cannot be null). */
export const UNPOSTED_MESSAGE_ID = '0';

const CUSTOM_EMOJI_RE = /^<(a)?:(\w{1,32}):(\d{17,20})>$/;

/**
 * The reaction key of a stored emoji: the custom emoji id, or the Unicode string without U+FE0F (emojiKeyOfStored).
 * @param {unknown} stored
 * @returns {string|null}
 */
export function emojiKey(stored) {
  if (typeof stored !== 'string' || stored === '') return null;
  const m = CUSTOM_EMOJI_RE.exec(stored);
  if (m) return m[3];
  return stored.replace(/\u{FE0F}/gu, '') || null;
}

const TYPE_OPTIONS = Object.freeze([
  { value: 'NORMAL', label: 'Normal (get and remove)' },
  { value: 'UNIQUE', label: 'Unique (one at a time)' },
  { value: 'VERIFY', label: 'Verify (get only)' },
  { value: 'REVERSE', label: 'Reverse (removes the role)' },
]);
const LAYOUT_OPTIONS = Object.freeze([
  { value: 'BUTTONS', label: 'Buttons' },
  { value: 'SELECT', label: 'Dropdown' },
  { value: 'REACTIONS', label: 'Emoji reactions' },
]);
const STYLE_OPTIONS = Object.freeze([
  { value: 1, label: 'Blue' },
  { value: 2, label: 'Grey' },
  { value: 3, label: 'Green' },
  { value: 4, label: 'Red' },
]);

// ── Cross-field rules ────────────────────────────────────────────────────────

const ATTACHED_TEXT = 'This panel uses an existing message that the bot never edits, so only the title, role mode, role limit and roles '
  + 'can change.';

/** Fields of an attached panel whose message the bot never edits. */
const LOCKED_WHEN_ATTACHED = Object.freeze(['description', 'messageContent', 'plainText']);

const sameValue = (a, b) => (a ?? null) === (b ?? null);

/**
 * The first problem of a role list for a layout, or null: the per-layout cap, one entry per role, and (emoji-reaction
 * panels) an emoji on every entry, each used once.
 * @param {Array<{ roleId: string, emoji?: string|null }>} items
 * @param {string} layout
 * @returns {{ path: string, message: string }|null}
 */
export function itemsProblem(items, layout) {
  const max = MAX_ITEMS[layout] ?? MAX_ITEMS.BUTTONS;
  if (items.length > max) {
    return { path: 'items', message: `A ${(LAYOUT_LABELS[layout] ?? 'Buttons').toLowerCase()} panel can hold at most ${max} roles.` };
  }
  const roles = new Set();
  const keys = new Set();
  for (const [i, item] of items.entries()) {
    if (roles.has(item.roleId)) {
      return { path: `items.${i}.roleId`, message: 'This role is already on the panel. Each role can be added once.' };
    }
    roles.add(item.roleId);
    if (layout !== 'REACTIONS') continue;
    const key = emojiKey(item.emoji);
    if (!key) return { path: `items.${i}.emoji`, message: 'Emoji-reaction panels need an emoji for each role.' };
    if (keys.has(key)) return { path: `items.${i}.emoji`, message: 'This emoji is already used for another role on this panel.' };
    keys.add(key);
  }
  return null;
}

function check(data, { mode, current }) {
  const layout = mode === 'create' ? data.layout : (current?.layout ?? 'BUTTONS');
  if (mode !== 'create' && current?.attached === true) {
    for (const field of LOCKED_WHEN_ATTACHED) {
      if (Object.hasOwn(data, field) && !sameValue(data[field], current[field])) return { path: field, message: ATTACHED_TEXT };
    }
  }
  if (Object.hasOwn(data, 'items')) return itemsProblem(data.items, layout);
  return null;
}

// ── The collection ───────────────────────────────────────────────────────────

const ITEM_FIELDS = Object.freeze([
  {
    field: 'roleId',
    type: 'role',
    label: 'Role',
    description: "The role members get, below the bot's highest role and yours.",
    assignable: true,
    required: true,
    default: null,
  },
  {
    field: 'label',
    type: 'text',
    label: 'Label',
    description: 'Button text or dropdown label, up to 80 characters. Empty: the role name.',
    maxLength: 80,
    nullable: true,
    default: null,
  },
  {
    field: 'emoji',
    type: 'emoji',
    label: 'Emoji',
    description: 'Required on emoji-reaction panels (each emoji once). A standard emoji or one of this server\'s emojis.',
    nullable: true,
    default: null,
  },
  {
    field: 'style',
    type: 'enum',
    label: 'Button colour',
    description: 'Button panels only.',
    options: STYLE_OPTIONS,
    default: 1,
  },
  {
    field: 'description',
    type: 'text',
    label: 'Dropdown description',
    description: 'Dropdown panels only, up to 100 characters.',
    maxLength: 100,
    nullable: true,
    default: null,
  },
]);

export const collection = Object.freeze({
  key: 'reactionRoles',
  module: 'reactionroles',
  label: 'Reaction role panels',
  singular: 'panel',
  description: 'Saving a panel posts it in its channel or updates its message, and deleting it deletes the message too. '
    + 'Panels made with /reactionrole attach stay attached: the bot never edits or deletes their message.',
  limit: LIMITS.REACTION_ROLE_PANELS,
  titleField: 'title',
  subtitleField: 'summary',
  fields: Object.freeze([
    {
      field: 'channelId',
      type: 'channel',
      label: 'Channel',
      description: 'Where the panel is posted. Use "Repost" to move it later.',
      channelTypes: [...POSTABLE_CHANNEL_KINDS],
      required: true,
      default: null,
    },
    {
      field: 'layout',
      type: 'enum',
      label: 'Layout',
      description: 'How members pick roles.',
      options: LAYOUT_OPTIONS,
      default: 'BUTTONS',
    },
    {
      field: 'type',
      type: 'enum',
      label: 'Role mode',
      description: 'Normal: get and remove roles. Unique: one role at a time. Verify: get roles only. Reverse: using a role removes it.',
      options: TYPE_OPTIONS,
      default: 'NORMAL',
    },
    {
      field: 'title',
      type: 'text',
      label: 'Title',
      description: 'The heading of the panel (up to 256 characters).',
      maxLength: 256,
      minLength: 1,
      default: DEFAULT_TITLE,
    },
    {
      field: 'description',
      type: 'text',
      label: 'Text under the title',
      description: 'Shown in the embed under the title. Empty: a default text for the layout.',
      multiline: true,
      maxLength: 4000,
      placeholders: false,
      nullable: true,
      default: null,
    },
    {
      field: 'messageContent',
      type: 'text',
      label: 'Message text',
      description: 'Plain text sent with the panel, up to 2000 characters.',
      multiline: true,
      maxLength: 2000,
      placeholders: false,
      nullable: true,
      default: null,
    },
    {
      field: 'plainText',
      type: 'boolean',
      label: 'Text only (no embed)',
      description: 'Send the panel as plain text instead of an embed.',
      default: false,
    },
    {
      field: 'maxRoles',
      type: 'int',
      label: 'Most roles per member',
      description: 'Normal mode only. Empty: no limit.',
      min: 1,
      max: MAX_MAX_ROLES,
      nullable: true,
      default: null,
      showWhen: { field: 'type', in: ['NORMAL'] },
    },
    {
      field: 'placeholder',
      type: 'text',
      label: 'Dropdown placeholder',
      description: 'Dropdown panels only: the text shown before a member picks, up to 150 characters. Empty: a default.',
      maxLength: 150,
      nullable: true,
      default: null,
      showWhen: { field: 'layout', in: ['SELECT'] },
    },
    {
      field: 'items',
      type: 'objectList',
      label: 'Roles',
      description: 'In this order. Buttons and dropdowns hold up to 25 roles, emoji reactions up to 20. Roles with moderator or admin '
        + 'permissions can only be added with /reactionrole add in Discord, which asks you to confirm.',
      itemLabel: 'Role',
      addLabel: 'Add role',
      emptyText: 'No roles yet. Add the roles members can pick.',
      itemFields: ITEM_FIELDS,
      maxItems: Math.max(...Object.values(MAX_ITEMS)),
      default: [],
    },
  ]),
  createOnly: Object.freeze(['channelId', 'layout']),
  // `summary` (the row's subtitle) and `attached` (read by the form rules) are also on every item.
  readOnly: Object.freeze([
    {
      field: 'status',
      type: 'text',
      label: 'Status',
      description: 'Posted, not posted yet (use "Repost"), message deleted in Discord (use "Repost"), or an attached message.',
      maxLength: 60,
      default: 'Posted',
    },
    { field: 'messageUrl', type: 'url', label: 'Panel message', description: 'Open the message in Discord.', nullable: true, default: null },
  ]),
  actions: Object.freeze([
    {
      id: 'repost',
      label: 'Repost',
      description: 'Posts the panel again, in another channel or after its message was deleted, and deletes the old message. '
        + 'An attached panel without a channel only gets its reactions back.',
      fields: [
        {
          field: 'channelId',
          type: 'channel',
          label: 'Channel',
          description: "A text or announcement channel the bot can post in. Empty: the panel's own channel.",
          channelTypes: [...POSTABLE_CHANNEL_KINDS],
          nullable: true,
          default: null,
        },
      ],
    },
  ]),
  check,
});

// ── Items ────────────────────────────────────────────────────────────────────

/**
 * What changing a panel's role list to `next` does to the stored rows. Entries are matched by role (one entry per role,
 * so the row's `rr:{groupId}:{roleId}` button id stays the same); the order of `next` is the new position.
 * `removedEmojis` are the emojis no entry uses any more: their reactions come off the panel message.
 * @param {Array<{ id: string, roleId: string, label: string, emoji: string|null, style: number, description: string|null,
 *   position: number }>} oldRows the stored rows
 * @param {Array<{ roleId: string, label: string, emoji: string|null, style: number, description: string|null }>} next
 * @returns {{ remove: object[], create: object[], update: Array<{ id: string, roleId: string, data: object }>,
 *   removedEmojis: string[], changed: boolean }}
 */
export function diffPanelItems(oldRows, next) {
  const byRole = new Map(oldRows.map((r) => [r.roleId, r]));
  const kept = new Set(next.map((n) => n.roleId));
  const remove = oldRows.filter((r) => !kept.has(r.roleId));
  const create = [];
  const update = [];
  next.forEach((n, position) => {
    const old = byRole.get(n.roleId);
    if (!old) {
      create.push({ ...n, position });
      return;
    }
    const data = {};
    for (const k of ['label', 'emoji', 'style', 'description']) if ((old[k] ?? null) !== (n[k] ?? null)) data[k] = n[k] ?? null;
    if (old.position !== position) data.position = position;
    if (Object.keys(data).length > 0) update.push({ id: old.id, roleId: n.roleId, data });
  });
  const stillUsed = new Set(next.map((n) => emojiKey(n.emoji)).filter(Boolean));
  const gone = [...remove.map((r) => r.emoji), ...update.filter((u) => 'emoji' in u.data).map((u) => byRole.get(u.roleId).emoji)];
  const removedEmojis = [];
  for (const emoji of gone) {
    const key = emojiKey(emoji);
    if (key && !stillUsed.has(key) && !removedEmojis.some((e) => emojiKey(e) === key)) removedEmojis.push(emoji);
  }
  return { remove, create, update, removedEmojis, changed: remove.length + create.length + update.length > 0 };
}

/**
 * The stored form of the submitted role list, as /reactionrole add makes it: the label falls back to the role name, the
 * button colour only counts for buttons, the description only for a dropdown, a custom emoji must be one of this server's
 * (or one the panel already holds). Then the per-layout rules again, now on the canonical emojis.
 * @throws {RecordError}
 */
export function normalizeItems(items, layout, resources, heldEmojiKeys = new Set()) {
  const roles = new Map((resources?.roles ?? []).map((r) => [r.id, r]));
  const emojis = new Map((resources?.emojis ?? []).map((e) => [e.id, e]));
  const out = items.map((item, i) => {
    let emoji = item.emoji ?? null;
    const custom = emoji ? CUSTOM_EMOJI_RE.exec(emoji) : null;
    if (custom) {
      const known = emojis.get(custom[3]);
      if (known) emoji = `<${known.animated ? 'a' : ''}:${known.name}:${known.id}>`;
      else if (!heldEmojiKeys.has(custom[3])) {
        throw new RecordError('invalid', "That custom emoji is not from this server. Pick one of this server's emojis or a standard emoji.", {
          path: `items.${i}.emoji`,
        });
      }
    }
    const label = clampText((item.label ?? '').trim() || roles.get(item.roleId)?.name || 'Role', 80);
    const description = layout === 'SELECT' && item.description ? clampText(item.description.trim(), 100) || null : null;
    return { roleId: item.roleId, label, emoji, style: layout === 'BUTTONS' ? item.style : 1, description };
  });
  const problem = itemsProblem(out, layout);
  if (problem) throw new RecordError('invalid', problem.message, { path: problem.path });
  return out;
}

/**
 * A ReactionRoleGroup row (with its `items`) as a collection item (wire shapes).
 * @param {object} row
 * @returns {object}
 */
export function panelItem(row) {
  const items = [...(row.items ?? [])]
    .sort((a, b) => (a.position ?? 0) - (b.position ?? 0))
    .map((i) => ({
      roleId: i.roleId,
      label: i.label,
      emoji: i.emoji ?? null,
      style: [1, 2, 3, 4].includes(i.style) ? i.style : 1,
      description: i.description ?? null,
    }));
  const unposted = row.messageId === UNPOSTED_MESSAGE_ID;
  let status = 'Posted';
  if (unposted) status = 'Not posted yet';
  else if (row.messageMissingAt) status = 'Message deleted';
  else if (row.attached) status = 'Attached message';
  const n = items.length;
  return {
    id: row.id,
    channelId: row.channelId,
    layout: row.layout,
    type: row.type,
    title: row.title,
    description: row.description ?? null,
    messageContent: row.messageContent ?? null,
    plainText: row.plainText === true,
    maxRoles: row.maxRoles ?? null,
    placeholder: row.placeholder ?? null,
    items,
    summary: `${LAYOUT_LABELS[row.layout] ?? row.layout} · ${TYPE_LABELS[row.type] ?? row.type} · ${n} ${n === 1 ? 'role' : 'roles'} · ${status}`,
    status,
    attached: row.attached === true,
    messageUrl: unposted ? null : messageUrl(row.guildId, row.channelId, row.messageId),
  };
}
