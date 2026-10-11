// The `giveaways` collection: the Giveaway rows behind /giveaway. Starting one posts the message with the Participate
// button, so every write goes through a dashboard task that runs the bot's own giveawayService (src/services/dashboardTasks/
// giveaways.js). What /giveaway offers is what the dashboard offers: start, end now, reroll, cancel, restart a draft and
// change the bonus odds of a running one. Everything else about a giveaway is fixed once it is posted, as in Discord.
// The list shows every status (running and draft ones first), so it has no "n / limit" count: the cap of
// LIMITS.ACTIVE_GIVEAWAYS counts only running and draft giveaways, and the bot enforces it when one is started.
// Pure: constants and helpers only; the limits that live in giveawayService.js are copied here and
// test/dashboard-giveaways-polls.test.js fails when they drift apart.
import { LIMITS } from '../../config/constants.js';
import { parseDuration } from '../../utils/parseDuration.js';
import { POSTABLE_CHANNEL_KINDS, messageUrl } from './embeds.js';

export const MIN_DURATION_MS = 60_000;
export const MAX_DURATION_MS = 90 * 86_400_000;
export const MAX_ROLE_WEIGHTS = 10;
export const MIN_BONUS_WEIGHT = 2;
export const MAX_WEIGHT = 10;
export const DEFAULT_BONUS_WEIGHT = 2;
export const MAX_PRIZE_AMOUNT = 1_000_000;
export const PRIZE_MAX = 256;
export const DESCRIPTION_MAX = 1000;
/** Most required (or denied) roles one giveaway lists; the service keeps arrays, the command takes one. */
export const MAX_GATE_ROLES = 10;

/** Row status -> label shown in the dashboard. */
export const STATUS_LABELS = Object.freeze({ ACTIVE: 'Running', DRAFT: 'Draft', ENDED: 'Ended', CANCELLED: 'Cancelled' });

const field = (f) => Object.freeze(f);

const fields = Object.freeze([
  field({
    field: 'prize',
    type: 'text',
    label: 'Prize',
    description: 'What the winners get. Shown as the title of the giveaway.',
    maxLength: PRIZE_MAX,
    minLength: 1,
    default: null,
    required: true,
  }),
  field({
    field: 'description',
    type: 'text',
    label: 'Description',
    description: 'Extra text shown in the giveaway message. Optional.',
    maxLength: DESCRIPTION_MAX,
    multiline: true,
    nullable: true,
    default: null,
  }),
  field({
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: 'Where the giveaway is posted. The bot needs to view the channel and send messages, embeds and message history there.',
    channelTypes: [...POSTABLE_CHANNEL_KINDS],
    default: null,
    required: true,
  }),
  field({
    field: 'winnerCount',
    type: 'int',
    label: 'Winners',
    description: `How many winners are drawn, 1 to ${LIMITS.GIVEAWAY_WINNERS}.`,
    min: 1,
    max: LIMITS.GIVEAWAY_WINNERS,
    default: 1,
  }),
  field({
    field: 'duration',
    type: 'text',
    label: 'Length',
    description: 'How long it runs, like 30m, 2h, 3d or 1w (1 minute to 90 days). Fill in this or the end time, not both.',
    maxLength: 40,
    nullable: true,
    default: null,
  }),
  field({
    field: 'endsAt',
    type: 'datetime',
    label: 'End time',
    description: 'Or the exact moment it ends (up to 90 days away). Fill in this or the length, not both.',
    nullable: true,
    default: null,
  }),
  field({
    field: 'requiredRoleIds',
    type: 'roles',
    label: 'Required roles',
    description: 'Members must have all of these roles to enter. Leave empty for everyone.',
    maxItems: MAX_GATE_ROLES,
    default: [],
  }),
  field({
    field: 'deniedRoleIds',
    type: 'roles',
    label: 'Blocked roles',
    description: 'Members with any of these roles cannot enter.',
    maxItems: MAX_GATE_ROLES,
    default: [],
  }),
  field({
    field: 'roleWeights',
    type: 'objectList',
    label: 'Bonus odds',
    description: 'Members with one of these roles count as several entries when the winners are drawn (the highest matching '
      + 'role counts). This is the one thing you can still change while a giveaway is running or in drafts.',
    maxItems: MAX_ROLE_WEIGHTS,
    default: [],
    itemFields: Object.freeze([
      field({ field: 'roleId', type: 'role', label: 'Role', required: true, default: null }),
      field({
        field: 'weight',
        type: 'int',
        label: 'Entries',
        description: 'How many entries each member of the role counts as.',
        min: MIN_BONUS_WEIGHT,
        max: MAX_WEIGHT,
        unit: 'entries',
        default: DEFAULT_BONUS_WEIGHT,
      }),
    ]),
  }),
  field({
    field: 'xpPrize',
    type: 'int',
    label: 'XP for each winner',
    description: 'Paid out through the Levels module when the giveaway ends. Needs that module to be on.',
    min: 1,
    max: MAX_PRIZE_AMOUNT,
    unit: 'XP',
    nullable: true,
    default: null,
  }),
  field({
    field: 'coinPrize',
    type: 'int',
    label: 'Coins for each winner',
    description: 'Paid out through the Economy module when the giveaway ends. Needs that module to be on.',
    min: 1,
    max: MAX_PRIZE_AMOUNT,
    unit: 'coins',
    nullable: true,
    default: null,
  }),
  field({
    field: 'pingRoleId',
    type: 'role',
    label: 'Role to ping',
    description: 'Mentioned when the giveaway is posted. The role must be mentionable, or the bot and you need Mention '
      + '@everyone, @here and All Roles in the channel.',
    nullable: true,
    default: null,
  }),
  field({
    field: 'embedColor',
    type: 'color',
    label: 'Colour',
    description: 'Colour of the giveaway message while it runs. Empty: the server colour.',
    format: 'int',
    min: 1,
    nullable: true,
    default: null,
  }),
]);

/** Everything but the bonus odds is fixed once the giveaway is posted. */
const CREATE_ONLY = Object.freeze(fields.map((f) => f.field).filter((f) => f !== 'roleWeights'));

const readOnly = Object.freeze([
  // The list line (status, end or winners, entries); the item also carries `status` on its own ('Running', 'Draft', ...).
  field({ field: 'summary', type: 'text', label: 'Status', maxLength: 200, default: '' }),
  field({ field: 'host', type: 'text', label: 'Hosted by', maxLength: 100, default: '' }),
  field({
    field: 'entries',
    type: 'int',
    label: 'Entries',
    description: 'Members who pressed Participate.',
    min: 0,
    max: 2_147_483_647,
    default: 0,
  }),
  field({
    field: 'winners', type: 'stringList', label: 'Winners', maxItems: LIMITS.GIVEAWAY_WINNERS * 10, itemMaxLength: 100, default: [],
  }),
  field({ field: 'endedAt', type: 'datetime', label: 'Ended', nullable: true, default: null }),
  field({ field: 'createdAt', type: 'datetime', label: 'Started', nullable: true, default: null }),
  field({ field: 'messageUrl', type: 'url', label: 'Message', maxLength: 200, nullable: true, default: null }),
]);

// A giveaway restarts into the same message layout; both ways to give the new end are optional (the old end is kept when
// it is still at least a minute away).
const restartFields = Object.freeze([
  field({
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: 'Where to post it again. Empty: the original channel.',
    channelTypes: [...POSTABLE_CHANNEL_KINDS],
    nullable: true,
    default: null,
  }),
  field({
    field: 'duration',
    type: 'text',
    label: 'New length',
    description: 'Like 30m, 2h or 3d. Leave both empty to keep the old end time (needs at least a minute left).',
    maxLength: 40,
    nullable: true,
    default: null,
  }),
  field({
    field: 'endsAt',
    type: 'datetime',
    label: 'New end time',
    description: 'Or the exact moment it ends. Use either this or the length.',
    nullable: true,
    default: null,
  }),
]);

export const collection = Object.freeze({
  key: 'giveaways',
  module: 'giveaways',
  label: 'Giveaways',
  singular: 'giveaway',
  description: 'Giveaways with a Participate button, the same as /giveaway in Discord. Running and draft ones come first, then '
    + `the finished ones. Up to ${LIMITS.ACTIVE_GIVEAWAYS} can be running at once. Only the bonus odds can be changed after `
    + 'it is posted. Starting and ending run in the bot, so they can take a few seconds.',
  limit: null,
  titleField: 'prize',
  subtitleField: 'summary',
  searchable: true,
  searchPlaceholder: 'Search by prize',
  canDelete: false,
  fields,
  createOnly: CREATE_ONLY,
  readOnly,
  actions: Object.freeze([
    Object.freeze({
      id: 'end',
      label: 'End now',
      description: 'Draws the winners now, announces them and pays the XP and coin prizes. Same as /giveaway end.',
      confirm: 'End this giveaway now and draw its winners?',
      fields: Object.freeze([]),
    }),
    Object.freeze({
      id: 'reroll',
      label: 'Reroll',
      description: 'Draws new winners of an ended giveaway. Earlier winners keep their prize. Same as /giveaway reroll.',
      fields: Object.freeze([
        field({
          field: 'winners',
          type: 'int',
          label: 'New winners',
          description: 'How many more winners to draw.',
          min: 1,
          max: LIMITS.GIVEAWAY_WINNERS,
          default: 1,
        }),
      ]),
    }),
    Object.freeze({
      id: 'restart',
      label: 'Restart',
      description: 'Posts a draft giveaway (its message was deleted) again. The entries are kept. Same as /giveaway restart.',
      fields: restartFields,
      check: (data) => endTimeProblem(data, { required: false }),
    }),
    Object.freeze({
      id: 'cancel',
      label: 'Cancel giveaway',
      description: 'Stops a running or draft giveaway without drawing winners. Same as /giveaway cancel.',
      danger: true,
      confirm: 'Cancel this giveaway? No winners will be drawn and this cannot be undone.',
      fields: Object.freeze([]),
    }),
  ]),
  check: checkGiveaway,
});

/**
 * The cross-field rules of the length / end time pair (the time rules of /giveaway start's `duration`).
 * @param {{ duration?: string|null, endsAt?: string|null }} data
 * @param {{ required?: boolean, now?: Date }} [opts] required: one of the two must be set
 * @returns {null|{ path: string, message: string }}
 */
export function endTimeProblem(data, { required = true, now = new Date() } = {}) {
  const duration = typeof data?.duration === 'string' && data.duration.trim() ? data.duration.trim() : null;
  const endsAt = data?.endsAt ?? null;
  if (!duration && !endsAt) {
    return required ? { path: 'duration', message: 'Set how long the giveaway runs, or when it ends.' } : null;
  }
  if (duration && endsAt) return { path: 'endsAt', message: 'Use either the length or the end time, not both.' };
  let ms;
  if (duration) {
    ms = parseDuration(duration);
    if (ms === null) return { path: 'duration', message: 'Use a length like 30m, 2h, 3d or 1w.' };
  } else {
    ms = Date.parse(endsAt) - now.getTime();
    if (!Number.isFinite(ms)) return { path: 'endsAt', message: 'Pick a valid end time.' };
    if (ms <= 0) return { path: 'endsAt', message: 'That end time is in the past.' };
  }
  const path = duration ? 'duration' : 'endsAt';
  if (ms < MIN_DURATION_MS) return { path, message: 'A giveaway must run for at least 1 minute.' };
  if (ms > MAX_DURATION_MS) return { path, message: 'A giveaway can run for at most 90 days.' };
  return null;
}

/** A role may carry bonus odds once (the service would silently keep the highest). */
function oddsProblem(list) {
  if (!Array.isArray(list)) return null;
  const seen = new Set();
  for (const [i, w] of list.entries()) {
    if (seen.has(w?.roleId)) return { path: `roleWeights.${i}.roleId`, message: 'Each role can have bonus odds only once.' };
    seen.add(w?.roleId);
  }
  return null;
}

/**
 * What /giveaway start and /giveaway odds refuse beyond the field rules: one end, no role both required and blocked, one
 * bonus per role. Updates only ever carry the bonus odds.
 * @returns {null|{ path: string, message: string }}
 */
export function checkGiveaway(data, { mode } = {}) {
  if (mode === 'update') return oddsProblem(data.roleWeights);
  const time = endTimeProblem(data);
  if (time) return time;
  const denied = new Set(data.deniedRoleIds ?? []);
  if ((data.requiredRoleIds ?? []).some((id) => denied.has(id))) {
    return { path: 'deniedRoleIds', message: 'The same role cannot be both required and blocked.' };
  }
  return oddsProblem(data.roleWeights);
}

// ── Items ────────────────────────────────────────────────────────────────────

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const utc = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC` : '?');
const count = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

/**
 * The list line under a giveaway's prize.
 * @param {{ status: string, endsAt?: Date, endedAt?: Date|null, winnerIds?: string[], _count?: { entries: number } }} row
 */
export function giveawaySummary(row) {
  const n = row._count?.entries ?? 0;
  const entries = n === 1 ? '1 entry' : `${n} entries`;
  switch (row.status) {
    case 'ACTIVE': return `Running, ends ${utc(row.endsAt)} · ${entries}`;
    case 'DRAFT': return `Draft (its message was deleted) · ${entries}`;
    case 'ENDED': return `Ended ${utc(row.endedAt ?? row.endsAt)} · ${count(row.winnerIds?.length ?? 0, 'winner')} · ${entries}`;
    default: return `Cancelled · ${entries}`;
  }
}

/**
 * A Giveaway row (with `_count.entries`) as a collection item (wire shapes).
 * @param {object} row
 * @param {{ names?: Map<string, string> }} [extra] display names of the host and the winners by user id
 */
export function giveawayItem(row, { names = new Map() } = {}) {
  const nameOf = (id) => names.get(id) ?? id;
  const weights = Array.isArray(row.roleWeights)
    ? row.roleWeights
      .filter((w) => w && typeof w.roleId === 'string' && Number.isInteger(w.weight))
      .map((w) => ({ roleId: w.roleId, weight: w.weight }))
    : [];
  return {
    id: row.id,
    prize: row.prize,
    description: row.description ?? null,
    channelId: row.channelId,
    winnerCount: row.winnerCount,
    duration: null,
    endsAt: iso(row.endsAt),
    requiredRoleIds: [...(row.requiredRoleIds ?? [])],
    deniedRoleIds: [...(row.deniedRoleIds ?? [])],
    roleWeights: weights,
    xpPrize: Number.isInteger(row.xpPrize) && row.xpPrize > 0 ? row.xpPrize : null,
    coinPrize: Number.isInteger(row.coinPrize) && row.coinPrize > 0 ? row.coinPrize : null,
    pingRoleId: row.pingRoleId ?? null,
    embedColor: Number.isInteger(row.embedColor) && row.embedColor > 0 ? row.embedColor : null,
    status: STATUS_LABELS[row.status] ?? String(row.status),
    summary: giveawaySummary(row),
    host: nameOf(row.hostId),
    entries: row._count?.entries ?? 0,
    winners: (row.winnerIds ?? []).map(nameOf),
    endedAt: iso(row.endedAt),
    createdAt: iso(row.createdAt),
    messageUrl: row.messageId ? messageUrl(row.guildId, row.channelId, row.messageId) : null,
  };
}
