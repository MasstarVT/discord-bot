// Descriptor of the `cases` collection (Moderation module): the server's moderation cases (Infraction rows), newest
// first. Creating a case performs the action with the same rules as the slash commands (/warn, /timeout, /unmute, /kick,
// /softban, /ban, /unban): the bot runs it through executeModAction with the dashboard user as the moderator (task
// `mod.action`, src/services/dashboardTasks/moderation.js). Editing changes what /case reason | proof change; the one
// destructive action is /case clear-history (pardon a member's warnings). Cases are never deleted. Pure.
import { formatDuration, parseDuration } from '../../utils/parseDuration.js';
import { avatarFor, profileLabel, profileName, toIso } from './levels.js';

// Mirrors of moderationService constants (test/dashboard-moderation.test.js keeps them equal).
/** Longest reason the action commands accept (their `reason` option). */
export const COMMAND_REASON_MAX = 512;
/** Longest stored reason and proof link (/case reason and proof). */
export const REASON_MAX = 1000;
export const TIMED_ACTION_MIN_MS = 60_000;
export const TIMED_ACTION_MAX_MS = 31_536_000_000;
export const DELETE_DAYS_MAX = 7;
export const MAX_CASE_ID = 2_147_483_647;

/** What the dashboard can do, in the order the form lists it (one slash command each). */
export const CASE_ACTIONS = Object.freeze([
  Object.freeze({ value: 'WARN', label: 'Warn', description: 'Records a warning and DMs the member (/warn). Warnings can trigger your automatic punishments.' }),
  Object.freeze({ value: 'TIMEOUT', label: 'Timeout', description: 'Times the member out (/timeout). Over 28 days uses the mute role, when one is set.' }),
  Object.freeze({ value: 'UNMUTE', label: 'Remove timeout or mute', description: "Removes the member's timeout and mute role (/unmute)." }),
  Object.freeze({ value: 'KICK', label: 'Kick', description: 'Kicks the member (/kick). They can rejoin with an invite.' }),
  Object.freeze({ value: 'SOFTBAN', label: 'Softban', description: 'Bans and immediately unbans to delete their recent messages (/softban).' }),
  Object.freeze({ value: 'BAN', label: 'Ban', description: 'Bans the user, also when they already left (/ban). Give a duration for a temporary ban.' }),
  Object.freeze({ value: 'UNBAN', label: 'Unban', description: 'Lifts a ban (/unban).' }),
]);
const ACTION_VALUES = CASE_ACTIONS.map((a) => a.value);
/** Actions whose command takes a proof link (/unban and /unmute have none). */
export const PROOF_ACTIONS = Object.freeze(['WARN', 'TIMEOUT', 'KICK', 'SOFTBAN', 'BAN']);

/** Labels of every case type (ACTION_META of moderationService; a BAN with an end is a temporary ban). */
export const CASE_TYPES = Object.freeze([
  { value: 'WARN', label: 'Warn' }, { value: 'TIMEOUT', label: 'Timeout' }, { value: 'MUTE', label: 'Mute' },
  { value: 'KICK', label: 'Kick' }, { value: 'SOFTBAN', label: 'Softban' }, { value: 'BAN', label: 'Ban' },
  { value: 'TEMPBAN', label: 'Temporary ban' }, { value: 'UNBAN', label: 'Unban' }, { value: 'UNMUTE', label: 'Unmute' },
  { value: 'NOTE', label: 'Note' },
].map(Object.freeze));
const TYPE_LABEL = Object.fromEntries(CASE_TYPES.map((t) => [t.value, t.label]));

export const CASE_STATUSES = Object.freeze([
  { value: 'active', label: 'Active' },
  { value: 'ending', label: 'Expired (the bot is lifting it)' },
  { value: 'expired', label: 'Expired' },
  { value: 'lifted', label: 'Lifted' },
  { value: 'pardoned', label: 'Pardoned' },
].map(Object.freeze));
const STATUS_LABEL = Object.fromEntries(CASE_STATUSES.map((s) => [s.value, s.label]));

const onlyFor = (...actions) => Object.freeze({ field: 'action', in: actions });

const fields = Object.freeze([
  Object.freeze({
    field: 'action',
    type: 'enum',
    label: 'Action',
    description: 'What to do. The same rules as the slash commands apply: your highest role must be above theirs and immune members are protected.',
    options: CASE_ACTIONS,
    default: 'WARN',
    required: true,
  }),
  Object.freeze({
    field: 'userId',
    type: 'user',
    label: 'Member',
    description: 'Who to act on. Bans and unbans also work for users who are not in the server: paste their user ID.',
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'reason',
    type: 'text',
    label: 'Reason',
    description: 'Kept with the case, written to the audit log and sent in the DM. Required for a warning; up to 512 characters '
      + '(1,000 when you edit it later, like /case reason).',
    maxLength: REASON_MAX,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'duration',
    type: 'text',
    label: 'Duration',
    description: 'How long, e.g. 10m, 2h, 7d or 1mo. A timeout needs one (over 28 days uses the mute role); leave it empty for a permanent ban.',
    maxLength: 32,
    nullable: true,
    default: null,
    showWhen: onlyFor('TIMEOUT', 'BAN'),
  }),
  Object.freeze({
    field: 'deleteDays',
    type: 'int',
    label: 'Delete their recent messages',
    description: 'Days of their messages to delete, 0 to 7.',
    min: 0,
    max: DELETE_DAYS_MAX,
    unit: 'days',
    default: 0,
    showWhen: onlyFor('BAN'),
  }),
  Object.freeze({
    field: 'softbanDeleteDays',
    type: 'int',
    label: 'Delete their recent messages',
    description: 'Days of their messages to delete, 1 to 7.',
    min: 1,
    max: DELETE_DAYS_MAX,
    unit: 'days',
    default: DELETE_DAYS_MAX,
    showWhen: onlyFor('SOFTBAN'),
  }),
  Object.freeze({
    field: 'proofUrl',
    type: 'url',
    label: 'Proof link',
    description: 'An http(s) link to evidence, kept with the case. It can be replaced later but not removed (like /case proof).',
    maxLength: REASON_MAX,
    nullable: true,
    default: null,
    showWhen: onlyFor(...PROOF_ACTIONS),
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'avatarUrl', type: 'url', label: 'Avatar', nullable: true, default: null }),
  Object.freeze({ field: 'title', type: 'text', label: 'Case', maxLength: 100, default: '' }),
  Object.freeze({ field: 'summary', type: 'text', label: 'Summary', maxLength: 300, default: '' }),
  Object.freeze({ field: 'caseNumber', type: 'int', label: 'Case number', min: 1, max: MAX_CASE_ID, default: 1 }),
  Object.freeze({ field: 'type', type: 'enum', label: 'Action taken', options: CASE_TYPES, default: 'WARN' }),
  Object.freeze({ field: 'member', type: 'text', label: 'Member', maxLength: 200, default: '' }),
  Object.freeze({ field: 'moderator', type: 'text', label: 'Moderator', maxLength: 200, default: '' }),
  Object.freeze({ field: 'status', type: 'enum', label: 'Status', options: CASE_STATUSES, nullable: true, default: null }),
  Object.freeze({ field: 'createdAt', type: 'datetime', label: 'Date', nullable: true, default: null }),
  Object.freeze({ field: 'expiresAt', type: 'datetime', label: 'Ends', nullable: true, default: null }),
]);

/** A duration problem of the create form, or null. `durationMs` is set on `out` when the text is valid. */
function durationProblem(action, text, out) {
  if (action !== 'TIMEOUT' && action !== 'BAN') return null;
  if (text === null || text === undefined) {
    return action === 'TIMEOUT' ? { path: 'duration', message: 'A timeout needs a duration, e.g. 10m, 2h or 7d.' } : null;
  }
  const ms = parseDuration(text);
  if (!ms) return { path: 'duration', message: `Couldn't read "${String(text).slice(0, 32)}". Use something like 12h, 7d or 1mo.` };
  if (ms < TIMED_ACTION_MIN_MS) return { path: 'duration', message: 'Durations must be at least 1 minute.' };
  if (ms > TIMED_ACTION_MAX_MS) {
    return { path: 'duration', message: action === 'BAN' ? 'Temporary bans can last at most 1 year.' : 'Mutes can last at most 1 year.' };
  }
  if (out) out.durationMs = ms;
  return null;
}

/**
 * The task payload of a create (the fields the chosen action takes; hidden form fields are ignored).
 * @param {object} data validated create data
 * @returns {{ action: string, userId: string, reason: string|null, durationMs: number|null, deleteMessageSeconds: number|null,
 *   proofUrl: string|null }}
 */
export function actionPayload(data) {
  const parsed = {};
  durationProblem(data.action, data.duration, parsed);
  let deleteDays = null;
  if (data.action === 'BAN') deleteDays = data.deleteDays ?? 0;
  if (data.action === 'SOFTBAN') deleteDays = data.softbanDeleteDays ?? DELETE_DAYS_MAX;
  return {
    action: data.action,
    userId: data.userId,
    reason: data.reason ?? null,
    durationMs: parsed.durationMs ?? null,
    deleteMessageSeconds: deleteDays === null ? null : deleteDays * 86_400,
    proofUrl: PROOF_ACTIONS.includes(data.action) ? data.proofUrl ?? null : null,
  };
}

export const collection = Object.freeze({
  key: 'cases',
  module: 'moderation',
  label: 'Moderation cases',
  singular: 'case',
  description: 'Every moderation action in this server, newest first. A new case warns, times out, kicks, softbans, bans or '
    + 'unbans someone exactly like the slash commands: the member gets a DM when that is on, the case is posted in the '
    + 'case log and warnings count towards your automatic punishments. Nothing is posted in a chat channel. Cases cannot be '
    + 'deleted; "Clear warnings" pardons all of a member\'s active warnings.',
  limit: null,
  searchable: true,
  searchPlaceholder: 'Search by member name, user ID or case number',
  titleField: 'title',
  subtitleField: 'summary',
  avatarField: 'avatarUrl',
  canDelete: false,
  fields,
  createOnly: Object.freeze(['action', 'userId', 'duration', 'deleteDays', 'softbanDeleteDays']),
  readOnly,
  actions: Object.freeze([
    Object.freeze({
      id: 'clearWarnings',
      label: 'Clear warnings',
      description: "Pardons all of this case's member's active warnings, like /case clear-history (administrators only). Their cases "
        + 'stay in the history, and escalation counting starts over.',
      danger: true,
      confirm: "Pardon every active warning of this case's member? Their cases stay in the history.",
      fields: Object.freeze([Object.freeze({
        field: 'clearReason',
        type: 'text',
        label: 'Reason',
        description: 'Why the warnings are cleared (kept in the pardon note).',
        maxLength: COMMAND_REASON_MAX,
        nullable: true,
        default: null,
      })]),
    }),
  ]),
  /** The rules of the commands that the field types cannot express. */
  check(data, { mode, current } = {}) {
    if (mode === 'create') {
      if (!ACTION_VALUES.includes(data.action)) return { path: 'action', message: 'Pick an action.' };
      if (data.action === 'WARN' && !data.reason) return { path: 'reason', message: 'A warning needs a reason.' };
      if (data.reason && data.reason.length > COMMAND_REASON_MAX) {
        return { path: 'reason', message: `The reason can be at most ${COMMAND_REASON_MAX} characters.` };
      }
      return durationProblem(data.action, data.duration, null);
    }
    if (mode === 'update') {
      if (Object.hasOwn(data, 'reason') && !data.reason) return { path: 'reason', message: "The reason can't be empty." };
      if (Object.hasOwn(data, 'proofUrl') && data.proofUrl === null && current?.proofUrl) {
        return { path: 'proofUrl', message: 'A proof link can be replaced but not removed.' };
      }
    }
    return null;
  },
});

// ── Items ────────────────────────────────────────────────────────────────────

/** Display type of a row: a BAN with an end is a temporary ban (displayType of moderationService). */
export const caseType = (row) => (row.type === 'BAN' && row.expiresAt ? 'TEMPBAN' : row.type);

/**
 * Where a case stands: active, ending (expired, the job lifts it soon), expired, lifted (ended early: unbanned, unmuted,
 * replaced by a newer timeout) or pardoned (a cleared warning). Null for one-off actions (kick, softban, unban, notes).
 * @param {{ type: string, active: boolean, expiresAt?: Date|null }} row
 * @param {number} [now]
 * @returns {string|null}
 */
export function caseStatus(row, now = Date.now()) {
  if (row.type === 'WARN') return row.active ? 'active' : 'pardoned';
  if (!['TIMEOUT', 'MUTE', 'BAN'].includes(row.type)) return null;
  const ended = row.expiresAt instanceof Date && row.expiresAt.getTime() <= now;
  if (row.active) return ended ? 'ending' : 'active';
  return ended ? 'expired' : 'lifted';
}

/** The create action closest to a stored case type, for the read-only "Action" of an existing case. */
const ACTION_OF_TYPE = Object.freeze({
  WARN: 'WARN', TIMEOUT: 'TIMEOUT', MUTE: 'TIMEOUT', UNMUTE: 'UNMUTE', KICK: 'KICK', SOFTBAN: 'SOFTBAN', BAN: 'BAN', UNBAN: 'UNBAN',
});

/** The length of a timed case, e.g. "1w 2d", from its end and creation (rounded to the minute). */
function caseDuration(row) {
  if (!(row.expiresAt instanceof Date) || !(row.createdAt instanceof Date)) return null;
  const ms = Math.round((row.expiresAt.getTime() - row.createdAt.getTime()) / 60_000) * 60_000;
  return ms > 0 ? formatDuration(ms) : null;
}

/** A stored reason on one line (the slash commands never store line breaks; old imports may). */
export const reasonText = (reason) => String(reason ?? '').replace(/\s*\n\s*/g, ' ').trim() || 'No reason provided';

/**
 * An Infraction row as a list item.
 * @param {object} row
 * @param {{ profiles?: Map<string, object|null>, now?: number }} [extra] profiles: ctx.discord.getUsers
 */
export function caseItem(row, { profiles = new Map(), now = Date.now() } = {}) {
  const target = profiles.get(row.targetUserId) ?? null;
  const moderator = profiles.get(row.moderatorId) ?? null;
  const type = caseType(row);
  const status = caseStatus(row, now);
  const created = toIso(row.createdAt);
  return {
    id: String(row.caseId),
    action: ACTION_OF_TYPE[row.type] ?? null,
    userId: row.targetUserId,
    reason: reasonText(row.reason),
    duration: caseDuration(row),
    deleteDays: null, // not stored
    softbanDeleteDays: null,
    proofUrl: row.proofUrl ?? null,
    avatarUrl: avatarFor(target, row.targetUserId),
    title: `#${row.caseId} · ${TYPE_LABEL[type] ?? type}`,
    summary: [profileName(target, row.targetUserId), status ? STATUS_LABEL[status] : null, created ? created.slice(0, 10) : null]
      .filter(Boolean).join(' · '),
    caseNumber: row.caseId,
    type,
    member: profileLabel(target, row.targetUserId),
    moderator: profileLabel(moderator, row.moderatorId),
    status,
    createdAt: created,
    expiresAt: toIso(row.expiresAt),
  };
}

/** A case number typed in the search box ("12" or "#12"), or null. */
export function caseNumberQuery(query) {
  const m = /^#?(\d{1,10})$/.exec(String(query ?? '').trim());
  if (!m) return null;
  const n = Number(m[1]);
  return n >= 1 && n <= MAX_CASE_ID ? n : null;
}
