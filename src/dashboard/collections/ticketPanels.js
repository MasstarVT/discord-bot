// The `ticketPanels` collection (Ticketing): ticket panels (TicketPanel rows) with their options (TicketPanelOption rows as
// the `options` list), manager roles, categories, transcripts, name format and messages, like /ticket-panel.
// Pure: constants and helpers only. The rules that live in ticketService.js (it opens Redis and Prisma at import) are copied
// here, and test/dashboard-panels.test.js fails when they drift apart.
import { LIMITS } from '../../config/constants.js';
import { emptyTemplate, normalizeTemplate } from '../../utils/customEmbed.js';
import { clampText } from '../../utils/text.js';
import { RecordError } from './errors.js';
import { POSTABLE_CHANNEL_KINDS, messageUrl } from './embeds.js';

// ── Rules copied from ticketService.js ───────────────────────────────────────

export const NAME_FORMAT_TOKENS = Object.freeze(['number', 'user.name', 'user.username', 'user.id', 'user.display_name', 'topic']);
export const DEFAULT_OPTION = Object.freeze({ label: 'Open Ticket', emoji: '✉️', buttonStyle: 1 });
export const MAX_OPEN_PER_USER = 10;
/** The `status` of a panel item. */
export const STATUS = Object.freeze({ PUBLISHED: 'Published', DRAFT: 'Not published' });

/** The panel message of a panel without its own: the heading differs only in the verb of its text. */
export function defaultPanelTemplate(style) {
  return {
    content: null,
    embed: {
      title: 'How can we help?',
      description: style === 'SELECT'
        ? 'Choose a topic below to open a private ticket with our team.'
        : 'Press a button below to open a private ticket with our team.',
    },
    buttons: [],
  };
}

/** The intro message of a new ticket when neither the option nor the panel has one. */
export const DEFAULT_INTRO_TEMPLATE = Object.freeze({
  content: '{user}',
  embed: Object.freeze({
    title: 'Ticket #{number}',
    description: 'Thanks for reaching out, {user.display_name}! A member of our team will be with you shortly.\nPlease describe your issue in as much detail as you can.',
  }),
  buttons: Object.freeze([]),
});

/** Discord-style channel name (ticketService.sanitizeChannelName). */
export function sanitizeChannelName(str) {
  let s = String(str ?? '').normalize('NFKC').toLowerCase();
  s = s.replace(/\s+/gu, '-').replace(/[^\p{L}\p{N}_-]/gu, '').replace(/-{2,}/g, '-').replace(/^[-_]+|[-_]+$/g, '');
  return clampText(s, 100, '');
}

/**
 * ticketService.validateNameFormat: 1-90 characters, only the known {variables}, at least one usable character.
 * @returns {{ ok: true, format: string } | { ok: false, error: string }}
 */
export function validateNameFormat(fmt) {
  const f = String(fmt ?? '').trim();
  if (!f || f.length > 90) return { ok: false, error: 'The name format must be 1–90 characters.' };
  for (const m of f.matchAll(/\{([^{}]*)\}/g)) {
    if (!NAME_FORMAT_TOKENS.includes(m[1].trim().toLowerCase())) {
      return { ok: false, error: `Unknown placeholder \`{${m[1]}}\`. Use {number}, {user.name}, {user.id}, {user.display_name} or {topic}.` };
    }
  }
  if (!sanitizeChannelName(f.replace(/\{[^{}]*\}/g, 'x'))) return { ok: false, error: 'The name format has no usable characters.' };
  return { ok: true, format: f };
}

// ── The collection ───────────────────────────────────────────────────────────

const STYLE_OPTIONS = Object.freeze([
  { value: 'BUTTONS', label: 'Buttons' },
  { value: 'SELECT', label: 'Dropdown' },
]);
const BUTTON_COLOUR_OPTIONS = Object.freeze([
  { value: 1, label: 'Blue' },
  { value: 2, label: 'Grey' },
  { value: 3, label: 'Green' },
  { value: 4, label: 'Red' },
]);

const OPTION_FIELDS = Object.freeze([
  {
    field: 'label',
    type: 'text',
    label: 'Label',
    description: 'The button text or dropdown entry, up to 80 characters. Each label once per panel.',
    maxLength: 80,
    minLength: 1,
    required: true,
    default: DEFAULT_OPTION.label,
  },
  {
    field: 'emoji',
    type: 'emoji',
    label: 'Emoji',
    description: 'A standard emoji or one of this server\'s custom emojis.',
    nullable: true,
    default: DEFAULT_OPTION.emoji,
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
  {
    field: 'buttonStyle',
    type: 'enum',
    label: 'Button colour',
    description: 'Button panels only.',
    options: BUTTON_COLOUR_OPTIONS,
    default: 1,
  },
  {
    field: 'categoryId',
    type: 'channel',
    label: 'Category for this option',
    description: 'Tickets of this option open here. Empty: the panel\'s open category.',
    channelTypes: ['category'],
    nullable: true,
    default: null,
  },
  {
    field: 'introTemplate',
    type: 'template',
    label: 'Intro message for this option',
    description: 'Posted in tickets of this option. Empty: the panel\'s intro message.',
    requireContent: false,
    placeholders: ['{number}', '{reason}', '{topic}', '{panel}', '{managers}'],
    default: emptyTemplate(),
  },
  {
    // The id of an existing option, kept so editing the list never replaces a row (its tickets stay linked). The form keeps
    // the value of this hidden field (showWhen with no accepted value) and sends it back unchanged.
    field: 'id',
    type: 'text',
    label: 'Option ID',
    description: 'Internal. Identifies an existing option when you edit the list.',
    maxLength: 40,
    pattern: '[A-Za-z0-9_-]+',
    nullable: true,
    default: null,
    showWhen: { field: 'label', in: [] },
  },
]);

const panelChannel = Object.freeze({
  field: 'channelId',
  type: 'channel',
  label: 'Publish in channel',
  description: 'Where the panel message is posted; pick another channel to move it. Empty: not published yet.',
  channelTypes: [...POSTABLE_CHANNEL_KINDS],
  nullable: true,
  default: null,
});

const check = (data, { mode, current }) => {
  if (Object.hasOwn(data, 'channelNameFormat')) {
    const v = validateNameFormat(data.channelNameFormat);
    if (!v.ok) return { path: 'channelNameFormat', message: v.error };
  }
  if (Object.hasOwn(data, 'options')) {
    const labels = new Set();
    const ids = new Set();
    for (const [i, option] of data.options.entries()) {
      const key = option.label.toLowerCase();
      if (labels.has(key)) return { path: `options.${i}.label`, message: `This panel already has an option called "${option.label}".` };
      labels.add(key);
      if (option.id) {
        if (ids.has(option.id)) return { path: `options.${i}.label`, message: 'That option is on the list twice.' };
        ids.add(option.id);
      }
    }
  }
  if (mode === 'update' && Object.hasOwn(data, 'channelId') && data.channelId === null && current?.channelId) {
    return {
      path: 'channelId',
      message: 'A published panel cannot be unpublished here. Pick another channel to move it, or delete the panel to remove its message.',
    };
  }
  return null;
};

export const collection = Object.freeze({
  key: 'ticketPanels',
  module: 'tickets',
  label: 'Ticket panels',
  singular: 'ticket panel',
  description: 'Saving a panel updates its published message, and deleting it deletes the message too (open tickets keep their '
    + 'channels). Publishing, categories, the transcript channel and manager roles need the bot to be online.',
  limit: LIMITS.TICKET_PANELS,
  titleField: 'name',
  subtitleField: 'summary',
  fields: Object.freeze([
    {
      field: 'name',
      type: 'text',
      label: 'Name',
      description: 'A name for this panel, unique on the server (up to 50 characters).',
      maxLength: 50,
      minLength: 1,
      required: true,
      default: null,
    },
    {
      field: 'style',
      type: 'enum',
      label: 'Style',
      description: 'Members open a ticket with buttons or with a dropdown menu.',
      options: STYLE_OPTIONS,
      default: 'BUTTONS',
    },
    panelChannel,
    {
      field: 'template',
      type: 'template',
      label: 'Panel message',
      description: 'The message with the buttons or dropdown. Link buttons are not used here. Empty: the default "How can we help?".',
      requireContent: false,
      placeholders: [],
      placeholderGroups: ['server', 'channel', 'other'],
      default: defaultPanelTemplate('BUTTONS'),
    },
    {
      field: 'options',
      type: 'objectList',
      label: 'Options',
      description: 'The buttons or dropdown entries, in this order. A panel keeps at least one.',
      itemLabel: 'Option',
      addLabel: 'Add option',
      itemFields: OPTION_FIELDS,
      maxItems: LIMITS.TICKET_PANEL_OPTIONS,
      minItems: 1,
      default: [{
        label: DEFAULT_OPTION.label,
        emoji: DEFAULT_OPTION.emoji,
        description: null,
        buttonStyle: DEFAULT_OPTION.buttonStyle,
        categoryId: null,
        introTemplate: emptyTemplate(),
        id: null,
      }],
    },
    {
      field: 'managerRoleIds',
      type: 'roles',
      label: 'Ticket manager roles',
      description: 'Roles that can see and handle this panel\'s tickets (moderators and admins always can). Existing tickets get access too.',
      maxItems: LIMITS.TICKET_MANAGER_ROLES,
      default: [],
    },
    {
      field: 'introTemplate',
      type: 'template',
      label: 'Ticket intro message',
      description: 'Posted when a ticket opens, with Close and Claim buttons. {managers} pings the manager roles. Empty: the default intro.',
      requireContent: false,
      placeholders: ['{number}', '{reason}', '{topic}', '{panel}', '{managers}'],
      default: DEFAULT_INTRO_TEMPLATE,
    },
    {
      field: 'openCategoryId',
      type: 'channel',
      label: 'Open tickets category',
      description: 'Where new tickets are created (the bot needs View Channel, Manage Channels and Manage Roles there). '
        + 'Empty: the bot creates a "Tickets" category when needed.',
      channelTypes: ['category'],
      nullable: true,
      default: null,
    },
    {
      field: 'claimedCategoryId',
      type: 'channel',
      label: 'Claimed tickets category',
      description: 'Where a claimed ticket moves. Empty: the bot creates "Claimed Tickets" when needed.',
      channelTypes: ['category'],
      nullable: true,
      default: null,
    },
    {
      field: 'closedCategoryId',
      type: 'channel',
      label: 'Closed tickets category',
      description: 'Where a closed ticket moves. Empty: the bot creates "Closed Tickets" when needed.',
      channelTypes: ['category'],
      nullable: true,
      default: null,
    },
    {
      field: 'transcriptChannelId',
      type: 'channel',
      label: 'Transcript channel',
      description: 'Where the HTML transcript is posted when a ticket is deleted (the bot needs Send Messages, Embed Links and Attach Files). '
        + 'Empty: none.',
      channelTypes: [...POSTABLE_CHANNEL_KINDS],
      nullable: true,
      default: null,
    },
    {
      field: 'dmTranscript',
      type: 'boolean',
      label: 'Send the transcript to the member',
      description: 'Also DM the transcript to the member who opened the ticket.',
      default: false,
    },
    {
      field: 'maxOpenPerUser',
      type: 'int',
      label: 'Open tickets per member',
      description: 'How many tickets a member can have open on this panel at once.',
      min: 1,
      max: MAX_OPEN_PER_USER,
      unit: 'tickets',
      default: 1,
    },
    {
      field: 'channelNameFormat',
      type: 'text',
      label: 'Ticket channel name',
      description: 'The name of new ticket channels, e.g. ticket-{number}. {topic} is the option label.',
      maxLength: 90,
      minLength: 1,
      placeholders: NAME_FORMAT_TOKENS.map((t) => `{${t}}`),
      placeholderGroups: [],
      default: 'ticket-{number}',
    },
    {
      field: 'askReason',
      type: 'boolean',
      label: 'Ask for a reason',
      description: 'Show a form asking why the member needs help before the ticket opens.',
      default: false,
    },
  ]),
  createOnly: Object.freeze([]),
  // `summary` (the row's subtitle) is also on every item.
  readOnly: Object.freeze([
    { field: 'status', type: 'text', label: 'Status', description: 'Whether the panel message is posted in a channel.', maxLength: 40, default: STATUS.DRAFT },
    { field: 'openTickets', type: 'int', label: 'Active tickets', description: 'Tickets of this panel that are open, claimed or closed.', min: 0, max: 1_000_000_000, default: 0 },
    { field: 'messageUrl', type: 'url', label: 'Panel message', description: 'Open the message in Discord.', nullable: true, default: null },
  ]),
  actions: Object.freeze([
    {
      id: 'publish',
      label: 'Publish',
      description: 'Posts the panel message in a channel, or updates it there. A message in another channel is deleted.',
      fields: [{
        field: 'channelId',
        type: 'channel',
        label: 'Channel',
        description: 'A text or announcement channel the bot can post in.',
        channelTypes: [...POSTABLE_CHANNEL_KINDS],
        required: true,
        default: null,
      }],
    },
    {
      id: 'refresh',
      label: 'Update posted message',
      // Offered only on a panel whose message is posted.
      showWhen: Object.freeze({ field: 'status', in: [STATUS.PUBLISHED] }),
      description: 'Applies the saved panel (message, style and options) to the published message again.',
      fields: [],
    },
  ]),
  check,
});

// ── Writing a list of options ────────────────────────────────────────────────

const hasVisibleEmbed = (e) => Boolean(e && (e.title || e.description || e.fields?.length || e.author?.name || e.footer?.text || e.image || e.thumbnail));

/** ticketService.isEmptyTemplate: an empty form resets a message to its default; a colour-only embed shows nothing. */
export function isEmptyTemplate(tpl) {
  const t = normalizeTemplate(tpl);
  return !(t.content && t.content.trim()) && !hasVisibleEmbed(t.embed);
}

/** A JSON-safe copy of a template for a Json column (no undefined values). */
export const toJson = (tpl) => JSON.parse(JSON.stringify(normalizeTemplate(tpl)));

const CUSTOM_EMOJI_RE = /^<(a)?:(\w{1,32}):(\d{17,20})>$/;

/**
 * An option emoji in its canonical form: a Unicode emoji as is, a custom emoji as the server writes it. A custom emoji must
 * be one of this server's (the bot can use it) unless the panel already holds it.
 * @param {string|null} emoji
 * @param {{ emojis?: Array<{ id: string, name: string, animated?: boolean }> }} resources getGuildResources
 * @param {Set<string>} heldIds custom emoji ids the stored options already use
 * @param {string} path error path
 * @returns {string|null}
 * @throws {RecordError}
 */
export function canonicalEmoji(emoji, resources, heldIds, path) {
  const custom = emoji ? CUSTOM_EMOJI_RE.exec(emoji) : null;
  if (!custom) return emoji ?? null;
  const known = (resources?.emojis ?? []).find((e) => e.id === custom[3]);
  if (known) return `<${known.animated ? 'a' : ''}:${known.name}:${known.id}>`;
  if (heldIds.has(custom[3])) return emoji;
  throw new RecordError('invalid', "That custom emoji is not from this server. Pick one of this server's emojis or a standard emoji.", { path });
}

/**
 * The stored form of the submitted options, as /ticket-panel option add | edit write them: an empty intro means "use the
 * panel's" (stored as null), the description is cut to 100 characters, custom emojis are checked.
 * @param {object[]} options validated list entries
 * @param {object} resources getGuildResources
 * @param {Set<string>} [heldEmojiIds]
 * @returns {Array<{ id: string|null, label: string, emoji: string|null, description: string|null, buttonStyle: number,
 *   categoryId: string|null, introTemplate: object|null }>}
 * @throws {RecordError}
 */
export function normalizeOptions(options, resources, heldEmojiIds = new Set()) {
  return options.map((o, i) => ({
    id: o.id ?? null,
    label: o.label,
    emoji: canonicalEmoji(o.emoji ?? null, resources, heldEmojiIds, `options.${i}.emoji`),
    description: o.description?.trim() ? clampText(o.description.trim(), 100) : null,
    buttonStyle: o.buttonStyle,
    categoryId: o.categoryId ?? null,
    introTemplate: isEmptyTemplate(o.introTemplate ?? emptyTemplate()) ? null : toJson(o.introTemplate),
  }));
}

const sameJson = (a, b) => JSON.stringify(a ? normalizeTemplate(a) : null) === JSON.stringify(b ? normalizeTemplate(b) : null);

/**
 * What changing a panel's options to `next` does to the stored rows. An entry keeps its row when it carries the row's id,
 * else when its label matches (case-insensitively) a row no entry claimed; every other entry is new and every unclaimed
 * row goes. The order of `next` is the new position.
 * @param {object[]} oldRows stored TicketPanelOption rows
 * @param {ReturnType<typeof normalizeOptions>} next
 * @returns {{ update: Array<{ id: string, data: object }>, create: object[], remove: object[], changed: boolean }}
 */
export function diffOptions(oldRows, next) {
  const claimed = new Set();
  const matched = new Array(next.length).fill(null);
  const byId = new Map(oldRows.map((r) => [r.id, r]));
  next.forEach((n, i) => {
    const row = n.id ? byId.get(n.id) : null;
    if (row && !claimed.has(row.id)) {
      matched[i] = row;
      claimed.add(row.id);
    }
  });
  next.forEach((n, i) => {
    if (matched[i]) return;
    const key = n.label.toLowerCase();
    const row = oldRows.find((r) => !claimed.has(r.id) && r.label.toLowerCase() === key);
    if (row) {
      matched[i] = row;
      claimed.add(row.id);
    }
  });
  const update = [];
  const create = [];
  next.forEach((n, position) => {
    const { id: _id, ...fields } = n;
    const row = matched[position];
    if (!row) {
      create.push({ ...fields, position });
      return;
    }
    const data = {};
    for (const k of ['label', 'emoji', 'description', 'buttonStyle', 'categoryId']) if ((row[k] ?? null) !== (fields[k] ?? null)) data[k] = fields[k] ?? null;
    if (!sameJson(row.introTemplate, fields.introTemplate)) data.introTemplate = fields.introTemplate;
    if (row.position !== position) data.position = position;
    if (Object.keys(data).length > 0) update.push({ id: row.id, data });
  });
  const remove = oldRows.filter((r) => !claimed.has(r.id));
  return { update, create, remove, changed: update.length + create.length + remove.length > 0 };
}

// ── Items ────────────────────────────────────────────────────────────────────

/** Whether a stored template is the panel's or intro's built-in default (so it can be stored as "not set"). */
export function isDefaultTemplate(template, defaults) {
  const a = JSON.stringify(normalizeTemplate(template));
  return (Array.isArray(defaults) ? defaults : [defaults]).some((d) => JSON.stringify(normalizeTemplate(d)) === a);
}

const byPosition = (a, b) => ((a.position ?? 0) - (b.position ?? 0)) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);

/**
 * A TicketPanelOption row as a list entry (wire shapes).
 * @param {object} row
 * @returns {object}
 */
export function optionItem(row) {
  return {
    label: row.label,
    emoji: row.emoji ?? null,
    description: row.description ?? null,
    buttonStyle: [1, 2, 3, 4].includes(row.buttonStyle) ? row.buttonStyle : 1,
    categoryId: row.categoryId ?? null,
    introTemplate: row.introTemplate ? normalizeTemplate(row.introTemplate) : emptyTemplate(),
    id: row.id,
  };
}

/**
 * A TicketPanel row (with its `options`) as a collection item (wire shapes).
 * @param {object} row
 * @param {number} [openTickets] tickets that are not deleted
 * @returns {object}
 */
export function panelItem(row, openTickets = 0) {
  const published = Boolean(row.channelId && row.messageId);
  const options = [...(row.options ?? [])].sort(byPosition).map(optionItem);
  const n = options.length;
  return {
    id: row.id,
    name: row.name,
    style: row.style,
    channelId: row.channelId ?? null,
    template: row.template ? normalizeTemplate(row.template) : defaultPanelTemplate(row.style),
    options,
    managerRoleIds: [...(row.managerRoleIds ?? [])],
    introTemplate: normalizeTemplate(row.introTemplate ?? DEFAULT_INTRO_TEMPLATE),
    openCategoryId: row.openCategoryId ?? null,
    claimedCategoryId: row.claimedCategoryId ?? null,
    closedCategoryId: row.closedCategoryId ?? null,
    transcriptChannelId: row.transcriptChannelId ?? null,
    dmTranscript: row.dmTranscript === true,
    maxOpenPerUser: row.maxOpenPerUser ?? 1,
    channelNameFormat: row.channelNameFormat ?? 'ticket-{number}',
    askReason: row.askReason === true,
    summary: `${row.style === 'SELECT' ? 'Dropdown' : 'Buttons'} · ${n} ${n === 1 ? 'option' : 'options'} · ${published ? STATUS.PUBLISHED : STATUS.DRAFT}`,
    status: published ? STATUS.PUBLISHED : STATUS.DRAFT,
    openTickets,
    messageUrl: published ? messageUrl(row.guildId, row.channelId, row.messageId) : null,
  };
}
