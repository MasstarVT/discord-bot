// The `polls` collection: the Poll rows behind /poll. Posting a poll sends the message with its vote buttons, so every
// write goes through a dashboard task that runs the bot's own pollService (src/services/dashboardTasks/polls.js). What
// /poll offers is what the dashboard offers: create (with the same options as the form), end now and list. A posted poll
// cannot be edited, as in Discord. "Delete" is the one addition: it removes the poll, its votes and its message, which is
// what a moderator deleting the poll message does (the poll ends without a results post) plus the record.
// The list shows every status (active ones first), so it has no "n / limit" count: the cap of LIMITS.ACTIVE_POLLS counts
// only active polls, and the bot enforces it when a poll is created.
// Pure: constants and helpers only; the limits that live in pollService.js are copied here and
// test/dashboard-giveaways-polls.test.js fails when they drift apart.
import { LIMITS } from '../../config/constants.js';
import { parseDuration } from '../../utils/parseDuration.js';
import { clampText } from '../../utils/text.js';
import { messageUrl } from './embeds.js';

export const QUESTION_MAX = 256;
export const OPTION_MAX = 80;
export const OPTIONS_MIN = 2;
export const DURATION_MIN_MS = 60_000;
export const DURATION_MAX_MS = 30 * 86_400_000;
/** Most allowed (or blocked) roles one poll lists; the service keeps arrays, the command takes one. */
export const MAX_GATE_ROLES = 10;

export const STATUS_LABELS = Object.freeze({ ACTIVE: 'Active', ENDED: 'Ended' });

/** /poll create accepts these channel kinds (and threads, which the dashboard never offers). */
export const POLL_CHANNEL_KINDS = Object.freeze(['text', 'announcement', 'voice']);

const field = (f) => Object.freeze(f);

const fields = Object.freeze([
  field({
    field: 'question',
    type: 'text',
    label: 'Question',
    description: 'Shown as the title of the poll.',
    maxLength: QUESTION_MAX,
    minLength: 1,
    default: null,
    required: true,
  }),
  field({
    field: 'options',
    type: 'stringList',
    label: 'Options',
    description: `${OPTIONS_MIN} to ${LIMITS.POLL_OPTIONS} answers, each up to ${OPTION_MAX} characters. Members vote with a button `
      + '(one choice) or a menu (several choices).',
    maxItems: LIMITS.POLL_OPTIONS,
    itemMaxLength: OPTION_MAX,
    default: [],
    required: true,
  }),
  field({
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: 'Where the poll is posted. The bot needs to view the channel and send messages and embeds there.',
    channelTypes: [...POLL_CHANNEL_KINDS],
    default: null,
    required: true,
  }),
  field({
    field: 'maxChoices',
    type: 'int',
    label: 'Choices per member',
    description: '1: one answer per member (buttons). More: each member can pick up to that many answers (a menu). '
      + 'It is lowered to the number of options when there are fewer.',
    min: 1,
    max: LIMITS.POLL_OPTIONS,
    default: 1,
  }),
  field({
    field: 'duration',
    type: 'text',
    label: 'Length',
    description: 'How long it runs, like 30m, 2h or 3d (1 minute to 30 days). Empty: until someone ends it.',
    maxLength: 32,
    nullable: true,
    default: null,
  }),
  field({
    field: 'allowedRoleIds',
    type: 'roles',
    label: 'Only these roles can vote',
    description: 'Leave empty so everyone can vote.',
    maxItems: MAX_GATE_ROLES,
    default: [],
  }),
  field({
    field: 'deniedRoleIds',
    type: 'roles',
    label: 'These roles cannot vote',
    description: 'Members with any of these roles cannot vote, even if they also have an allowed role.',
    maxItems: MAX_GATE_ROLES,
    default: [],
  }),
  field({
    field: 'hideResults',
    type: 'boolean',
    label: 'Hide results until it ends',
    description: 'Members see no counts while the poll is active. You can always see them here.',
    default: false,
  }),
]);

const readOnly = Object.freeze([
  // The list line (status, end, votes, who leads); the item also carries `status` on its own ('Active' or 'Ended').
  field({ field: 'summary', type: 'text', label: 'Status', maxLength: 240, default: '' }),
  field({
    field: 'results', type: 'stringList', label: 'Votes per option', maxItems: LIMITS.POLL_OPTIONS, itemMaxLength: 200, default: [],
  }),
  field({ field: 'votes', type: 'int', label: 'Votes', min: 0, max: 2_147_483_647, default: 0 }),
  field({ field: 'voters', type: 'int', label: 'Members who voted', min: 0, max: 2_147_483_647, default: 0 }),
  field({ field: 'endsAt', type: 'datetime', label: 'Ends / ended', nullable: true, default: null }),
  field({ field: 'author', type: 'text', label: 'Started by', maxLength: 100, default: '' }),
  field({ field: 'createdAt', type: 'datetime', label: 'Started', nullable: true, default: null }),
  field({ field: 'messageUrl', type: 'url', label: 'Message', maxLength: 200, nullable: true, default: null }),
]);

export const collection = Object.freeze({
  key: 'polls',
  module: 'polls',
  label: 'Polls',
  singular: 'poll',
  description: 'Polls with vote buttons or a menu, the same as /poll in Discord. Active ones come first, then the finished '
    + `ones. Up to ${LIMITS.ACTIVE_POLLS} can be active at once. A posted poll cannot be edited. Posting and ending run in `
    + 'the bot, so they can take a few seconds.',
  limit: null,
  titleField: 'question',
  subtitleField: 'summary',
  searchable: true,
  searchPlaceholder: 'Search by question',
  canEdit: false,
  fields,
  createOnly: Object.freeze(fields.map((f) => f.field)),
  readOnly,
  actions: Object.freeze([
    Object.freeze({
      id: 'end',
      label: 'End now',
      description: 'Closes the poll and posts the results in its channel. Same as /poll end.',
      confirm: 'End this poll now and post the results?',
      fields: Object.freeze([]),
    }),
  ]),
  check: checkPoll,
});

/** The cross-field rules of the length (parseDurationOption in pollService.js). */
export function durationProblem(duration) {
  const text = typeof duration === 'string' ? duration.trim() : '';
  if (!text) return null;
  const ms = parseDuration(text);
  if (ms === null) return { path: 'duration', message: 'Use a length like 30m, 2h, 1d or 1w.' };
  if (ms < DURATION_MIN_MS) return { path: 'duration', message: 'Polls must run for at least 1 minute.' };
  if (ms > DURATION_MAX_MS) return { path: 'duration', message: 'Polls can run for at most 30 days.' };
  return null;
}

/**
 * What /poll create refuses beyond the field rules: fewer than two different options, an allowed role that is also blocked.
 * @returns {null|{ path: string, message: string }}
 */
export function checkPoll(data) {
  if ((data.options ?? []).length < OPTIONS_MIN) {
    return { path: 'options', message: `Add at least ${OPTIONS_MIN} different options (a repeated option counts once).` };
  }
  const time = durationProblem(data.duration);
  if (time) return time;
  const denied = new Set(data.deniedRoleIds ?? []);
  if ((data.allowedRoleIds ?? []).some((id) => denied.has(id))) {
    return { path: 'deniedRoleIds', message: 'The allowed roles and the blocked roles must be different.' };
  }
  return null;
}

// ── Items ────────────────────────────────────────────────────────────────────

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const utc = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC` : '?');
const percent = (n, total) => (total ? Math.round((n * 100) / total) : 0);

/** Who is ahead: "leading: Margherita (56%)", "won: ..." once ended, "tie: A, B"; null before the first vote. */
function leadText(options, counts, votes, ended) {
  const top = Math.max(0, ...counts);
  if (!votes || !top) return null;
  const names = options.filter((_, i) => counts[i] === top).map((o) => clampText(o, 40));
  if (names.length === 1) return `${ended ? 'won' : 'leading'}: ${names[0]} (${percent(top, votes)}%)`;
  return `tie: ${names.slice(0, 3).join(', ')}${names.length > 3 ? ` +${names.length - 3}` : ''}`;
}

/**
 * The list line under a poll's question.
 * @param {{ status: string, endsAt?: Date|null, options?: string[] }} row
 * @param {{ votes: number, counts?: number[] }} tally
 */
export function pollSummary(row, tally) {
  const ended = row.status === 'ENDED';
  const parts = [
    ended ? `Ended ${utc(row.endsAt)}` : `Active, ${row.endsAt ? `ends ${utc(row.endsAt)}` : 'open until ended'}`,
    tally.votes === 1 ? '1 vote' : `${tally.votes} votes`,
    leadText(row.options ?? [], tally.counts ?? [], tally.votes, ended),
  ];
  return parts.filter(Boolean).join(' · ');
}

/**
 * A Poll row as a collection item (wire shapes).
 * @param {object} row
 * @param {{ tally?: { counts: number[], votes: number, voters: number }, names?: Map<string, string> }} [extra]
 */
export function pollItem(row, { tally = null, names = new Map() } = {}) {
  const options = Array.isArray(row.options) ? row.options : [];
  const counts = options.map((_, i) => tally?.counts?.[i] ?? 0);
  const votes = Math.max(tally?.votes ?? 0, counts.reduce((a, b) => a + b, 0));
  return {
    id: row.id,
    question: row.question,
    options: [...options],
    channelId: row.channelId,
    maxChoices: row.maxChoices,
    duration: null,
    allowedRoleIds: [...(row.allowedRoleIds ?? [])],
    deniedRoleIds: [...(row.deniedRoleIds ?? [])],
    hideResults: row.hideResults === true,
    status: STATUS_LABELS[row.status] ?? String(row.status),
    summary: pollSummary(row, { votes, counts }),
    results: options.map((o, i) => `${i + 1}. ${o}: ${counts[i] === 1 ? '1 vote' : `${counts[i]} votes`} (${percent(counts[i], votes)}%)`),
    votes,
    voters: tally?.voters ?? 0,
    endsAt: iso(row.endsAt),
    author: names.get(row.authorId) ?? row.authorId,
    createdAt: iso(row.createdAt),
    messageUrl: row.messageId ? messageUrl(row.guildId, row.channelId, row.messageId) : null,
  };
}
