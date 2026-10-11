// Descriptor of the `statChannels` collection: voice channels the bot renames to show a live number (/stats create|edit|delete).
// Pure: constants only; KIND_INFO is a copy of the one in statsService.js (the service opens Redis and Prisma at import) and
// test/dashboard-collections-more.test.js fails when the two drift apart.
import { LIMITS } from '../../config/constants.js';

/** The counter types (= the Prisma enum StatKind, same order) with their label and default name template. */
export const KIND_INFO = Object.freeze({
  MEMBERS: Object.freeze({ label: 'Members', template: 'Members: {count}' }),
  HUMANS: Object.freeze({ label: 'Humans', template: 'Humans: {count}' }),
  BOTS: Object.freeze({ label: 'Bots', template: 'Bots: {count}' }),
  ONLINE: Object.freeze({ label: 'Online', template: 'Online: {count}' }),
  PEAK_ONLINE: Object.freeze({ label: 'Peak online', template: 'Peak Online: {count}' }),
  TEXT_CHANNELS: Object.freeze({ label: 'Text channels', template: 'Text Channels: {count}' }),
  VOICE_CHANNELS: Object.freeze({ label: 'Voice channels', template: 'Voice Channels: {count}' }),
  TOTAL_CHANNELS: Object.freeze({ label: 'All channels', template: 'Channels: {count}' }),
  ROLES: Object.freeze({ label: 'Roles', template: 'Roles: {count}' }),
  ROLE_MEMBERS: Object.freeze({ label: 'Role members', template: '{role}: {count}' }),
  BOOSTS: Object.freeze({ label: 'Boosts', template: 'Boosts: {count}' }),
});

export const MAX_NAME_LENGTH = 100;

/** Collapses whitespace like normalizeTemplate in statsService.js. */
export const normalizeTemplate = (str) => String(str ?? '').replace(/\s+/g, ' ').trim();

const fields = Object.freeze([
  Object.freeze({
    field: 'kind',
    type: 'enum',
    label: 'What to count',
    description: 'The number the channel name shows. Cannot be changed later. '
      + 'Only the Online and Peak online counters need the bot to see presences.',
    options: Object.freeze(Object.entries(KIND_INFO).map(([value, info]) => Object.freeze({ value, label: info.label }))),
    default: 'MEMBERS',
    required: true,
  }),
  Object.freeze({
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: 'An existing voice channel that the bot renames. Pick a dedicated one: its name is replaced. '
      + 'Deny Connect for @everyone on it so nobody joins (/stats create in Discord does that for you). It cannot be changed later.',
    channelTypes: Object.freeze(['voice']),
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'roleId',
    type: 'role',
    label: 'Role to count',
    description: 'Only for "Role members": whose members are counted. The role is only counted, never given. Cannot be changed later.',
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'template',
    type: 'text',
    label: 'Channel name',
    description: 'The channel name; {count} is replaced by the number ({role} by the role name for "Role members"). '
      + 'Empty uses the default for the counter type, e.g. "Members: {count}".',
    maxLength: MAX_NAME_LENGTH,
    nullable: true,
    default: null,
    placeholders: Object.freeze(['{count}', '{role}']),
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'kindLabel', type: 'text', label: 'Counter', maxLength: 40, default: '' }),
  Object.freeze({
    field: 'lastValue', type: 'int', label: 'Showing', description: 'The number in the channel name at the last update.',
    min: 0, max: 2_147_483_647, nullable: true, default: null,
  }),
  Object.freeze({
    field: 'peakValue', type: 'int', label: 'Peak', description: 'The highest online count seen (Peak online only).',
    min: 0, max: 2_147_483_647, nullable: true, default: null,
  }),
  Object.freeze({ field: 'lastRenamedAt', type: 'datetime', label: 'Last renamed', nullable: true, default: null }),
]);

export const collection = Object.freeze({
  key: 'statChannels',
  module: 'stats',
  label: 'Counter channels',
  singular: 'counter channel',
  description: 'Voice channels renamed about every 10 minutes to show a live number (members, online, boosts, ...). '
    + 'Discord allows only a few renames per channel, so a name can lag a few minutes. '
    + 'Deleting a counter here only stops the updates; the channel stays in Discord with its last name. '
    + 'Channels that /stats create makes (locked for @everyone) can be edited and deleted here.',
  limit: LIMITS.STAT_CHANNELS,
  titleField: 'kindLabel',
  subtitleField: 'template',
  fields,
  createOnly: Object.freeze(['kind', 'channelId', 'roleId']),
  readOnly,
  actions: Object.freeze([
    Object.freeze({
      id: 'refresh',
      label: 'Update the counters now',
      description: 'Asks the bot to update every counter channel of this server within about a minute (/stats refresh). '
        + "Discord still limits how often a channel can be renamed, so a name can lag a few minutes. The Statistics Channels module must be on.",
      fields: Object.freeze([]),
    }),
  ]),
  check(data, { mode, current } = {}) {
    const kind = data.kind ?? current?.kind;
    if (mode === 'create') {
      if (kind === 'ROLE_MEMBERS' && !data.roleId) return { path: 'roleId', message: 'Pick the role to count for "Role members".' };
      if (kind !== 'ROLE_MEMBERS' && data.roleId) return { path: 'roleId', message: 'The role only applies to the "Role members" counter.' };
    }
    if (typeof data.template === 'string') {
      const t = normalizeTemplate(data.template);
      if (!/\{count\}/i.test(t)) return { path: 'template', message: 'The name must contain {count}, e.g. "Members: {count}".' };
      if (/\{random/i.test(t)) {
        return { path: 'template', message: 'Random variables are not allowed: the name would change on every refresh.' };
      }
      if (/\{role\}/i.test(t) && kind !== 'ROLE_MEMBERS') {
        return { path: 'template', message: '{role} only works with the "Role members" counter.' };
      }
    }
    return null;
  },
});
