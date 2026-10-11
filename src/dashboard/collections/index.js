// The registry of per-server record collections the dashboard manages (saved embeds, custom commands, level rewards,
// ...). Each collection is described by `src/dashboard/collections/<key>.js` (pure: descriptors, limits, rules) and
// stored by `<key>.store.js` (database access, loaded only through src/dashboard/recordsBridge.js).
// Pure: importing this file opens no connection.
import { RecordError } from './errors.js';
import { FieldError, emptyRefs, isPlainObject, lintDescriptors, publicDescriptor, validateValue } from '../schema.js';
import { collection as embeds } from './embeds.js';
import { collection as tags } from './tags.js';
import { collection as triggers } from './triggers.js';
import { collection as reminders } from './reminders.js';
import { collection as feeds } from './feeds.js';
import { collection as levelRoles } from './levelRoles.js';
import { collection as shopItems } from './shopItems.js';
import { collection as achievements } from './achievements.js';
import { collection as starboards } from './starboards.js';
import { collection as translateChannels } from './translateChannels.js';
import { collection as tempVoiceHubs } from './tempVoiceHubs.js';
import { collection as statChannels } from './statChannels.js';
import { collection as economyBoosts } from './economyBoosts.js';

/** Display order. */
export const COLLECTION_LIST = Object.freeze([
  embeds, tags, triggers, reminders, feeds, levelRoles, shopItems, achievements, starboards, translateChannels,
  tempVoiceHubs, statChannels, economyBoosts,
]);

/** key -> collection descriptor. */
export const COLLECTIONS = Object.freeze(Object.fromEntries(COLLECTION_LIST.map((c) => [c.key, c])));

export const COLLECTION_KEY_RE = /^[A-Za-z]{1,32}$/;
export const RECORD_ID_RE = /^[A-Za-z0-9_-]{1,40}$/;

const FORBIDDEN_KEYS = new Set(['__proto__', 'constructor', 'prototype']);

/**
 * A collection descriptor as the browser gets it: server-only hooks removed, descriptors made JSON-safe.
 * @param {object} c
 * @returns {Readonly<object>}
 */
export function publicCollection(c) {
  return Object.freeze({
    key: c.key,
    module: c.module,
    label: c.label,
    singular: c.singular,
    description: c.description ?? '',
    limit: c.limit,
    titleField: c.titleField,
    subtitleField: c.subtitleField ?? null,
    canCreate: c.canCreate !== false,
    canEdit: c.canEdit !== false,
    canDelete: c.canDelete !== false,
    fields: Object.freeze(c.fields.map(publicDescriptor)),
    createOnly: Object.freeze([...(c.createOnly ?? [])]),
    readOnly: Object.freeze((c.readOnly ?? []).map(publicDescriptor)),
    actions: Object.freeze((c.actions ?? []).map((a) => Object.freeze({
      id: a.id,
      label: a.label,
      description: a.description ?? '',
      danger: a.danger === true,
      confirm: a.confirm ?? null,
      fields: Object.freeze((a.fields ?? []).map(publicDescriptor)),
    }))),
  });
}

const asFieldError = (problem) => (typeof problem === 'object'
  ? new FieldError(problem.path ?? '', problem.message)
  : new FieldError('', problem));

/**
 * Validates the `data` of a create, update or action request against a collection's descriptors.
 *   create: every field is validated; a missing field takes its default (or fails when `required`).
 *   update: only the fields present are validated; createOnly and unknown fields are refused.
 *   action: the fields of that action (`actionId`) are validated like a create.
 * Then the collection's `check(data, { mode, current })` (or the action's `check(data, { current })`) runs; it
 * returns null, an error message, or `{ path, message }`.
 * @param {string} key collection key
 * @param {unknown} data the request's `data`
 * @param {{ mode: 'create'|'update'|'action', actionId?: string, current?: object|null }} opts
 *   current: the stored item (update and action)
 * @returns {{ data: object, refs: import('../schema.js').Refs }}
 * @throws {FieldError|RecordError}
 */
export function validateRecordInput(key, data, { mode, actionId, current = null }) {
  const c = COLLECTIONS[key];
  if (!c) throw new RecordError('not_found', 'Unknown list.');
  if (!isPlainObject(data)) throw new FieldError('', '"data" must be an object.');
  const action = mode === 'action' ? (c.actions ?? []).find((a) => a.id === actionId) : null;
  if (mode === 'action' && !action) throw new RecordError('not_found', 'Unknown action.');
  const fields = action ? action.fields ?? [] : c.fields;

  const known = new Map(fields.map((f) => [f.field, f]));
  for (const k of Object.keys(data)) {
    if (FORBIDDEN_KEYS.has(k) || !known.has(k)) throw new FieldError(k.slice(0, 40), `"${k.slice(0, 40)}" is not something you can set here.`);
    if (mode === 'update' && (c.createOnly ?? []).includes(k)) throw new FieldError(k, `"${known.get(k).label}" cannot be changed after creation.`);
  }
  const refs = emptyRefs();
  const out = {};
  for (const f of fields) {
    if (Object.hasOwn(data, f.field)) out[f.field] = validateValue(f, data[f.field], f.field, refs);
    else if (mode !== 'update') {
      if (f.required) throw new FieldError(f.field, `"${f.label}" is required.`);
      out[f.field] = f.default === undefined ? null : structuredClone(f.default);
    }
  }
  const check = action ? action.check : c.check;
  if (typeof check === 'function') {
    const problem = action ? check(out, { current }) : check(out, { mode, current });
    if (problem) throw asFieldError(problem);
  }
  return { data: out, refs };
}

/**
 * Descriptor problems of every collection (tests assert this is empty).
 * @returns {string[]}
 */
export function lintCollections() {
  const problems = [];
  for (const c of COLLECTION_LIST) {
    if (!COLLECTION_KEY_RE.test(c.key)) problems.push(`${c.key}: bad key`);
    problems.push(...lintDescriptors(c.fields, `${c.key}.`));
    problems.push(...lintDescriptors(c.readOnly ?? [], `${c.key}(readOnly).`).filter((p) => !p.includes('the default')));
    for (const a of c.actions ?? []) problems.push(...lintDescriptors(a.fields ?? [], `${c.key}!${a.id}.`));
    for (const f of c.createOnly ?? []) if (!c.fields.some((d) => d.field === f)) problems.push(`${c.key}: createOnly ${f} is not a field`);
    const titled = [...c.fields, ...(c.readOnly ?? [])].some((d) => d.field === c.titleField);
    if (!c.placeholder && !titled && c.titleField !== 'id') problems.push(`${c.key}: titleField ${c.titleField} is not a field`);
  }
  return problems;
}
