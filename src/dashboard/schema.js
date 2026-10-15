// The dashboard's field descriptors: one declarative description per editable value (a GuildSettings column, a
// part of a JSON column, or a field of a per-server record), a strict validator and a lenient reader for each,
// and the check of channel and role ids against the server's real channels and roles.
// The same descriptors are served to the browser (publicDescriptor), which renders a form from them.
// Pure: imports only pure helpers, nothing is read or connected at import time.
import { isHttpUrl, normalizeTemplate, validateTemplate } from '../utils/customEmbed.js';

/**
 * Every descriptor type. Common keys: `field` (the key in its parent object), `type`, `label`, `description?`,
 * `default` (JSON value used when the stored value is missing or malformed), `nullable?` (null is accepted and
 * means "not set"), `required?` (objects only: the key must be present).
 *
 *   boolean                         strict true/false
 *   int      min max unit?          an integer in [min, max]; unit is a display hint ('seconds', 'minutes', ...)
 *   float    min max step?          a finite number in [min, max]
 *   text     maxLength minLength? multiline? placeholders? pattern?
 *                                   a string; with `nullable` an empty or blank string is stored as null.
 *                                   `pattern` is a RegExp source the whole value must match (flags 'u').
 *   enum     options                one of options[].value
 *   multiEnum options maxItems?     unique values from options[].value
 *   color    format ('int'|'hex') min?
 *                                   accepts '#RRGGBB', 'RRGGBB' or an integer; stored as an int or '#RRGGBB'
 *   timezone                        an IANA zone name or 'UTC'
 *   channel  channelTypes           a channel id of this server whose kind is listed (CHANNEL_KINDS keys)
 *   channels channelTypes maxItems  unique channel ids
 *   role     assignable? allowEveryone? allowDangerous?
 *                                   a role id of this server; assignable: the bot must be able to give it (and, unless
 *                                   allowDangerous, the role carries no moderator/admin permissions)
 *   roles    assignable? allowEveryone? allowDangerous? maxItems
 *   user                            a user id (the browser offers a member search, /api/guilds/:id/members);
 *                                   whether that user is in the server is checked by the code that acts on it
 *   template requireContent?        a MessageTemplate { content, embed, buttons } (src/utils/customEmbed.js);
 *                                   requireContent (default true): an empty message is refused
 *   embed                           an EmbedSpec (the `embed` of a template) or null
 *   stringList maxItems itemMaxLength itemMinLength? lowercase? itemPattern?
 *                                   unique trimmed strings (case-insensitive unique when lowercase)
 *   object   fields                 a plain object; every child is validated, missing children take
 *                                   their default, unknown keys are refused
 *   objectList itemFields maxItems minItems?
 *                                   an array of objects shaped like `object`
 *   datetime                        an ISO-8601 date-time, stored as an ISO string (UTC)
 *   time                            'HH:MM', 24 h
 *   emoji                           one Unicode emoji or a custom emoji '<:name:id>' / '<a:name:id>'
 *   url      maxLength              an absolute http(s) URL
 */
export const FIELD_TYPES = Object.freeze([
  'boolean', 'int', 'float', 'text', 'enum', 'multiEnum', 'color', 'timezone', 'channel', 'channels', 'role', 'roles',
  'user', 'template', 'embed', 'stringList', 'object', 'objectList', 'datetime', 'time', 'emoji', 'url',
]);

/** Discord channel type numbers per channel kind (threads are never offered). */
export const CHANNEL_KINDS = Object.freeze({
  text: Object.freeze([0]),
  voice: Object.freeze([2]),
  category: Object.freeze([4]),
  announcement: Object.freeze([5]),
  stage: Object.freeze([13]),
  forum: Object.freeze([15]),
  media: Object.freeze([16]),
});

/** The kind of a Discord channel type number, or 'other'. */
export function channelKind(type) {
  for (const [kind, types] of Object.entries(CHANNEL_KINDS)) if (types.includes(type)) return kind;
  return 'other';
}

const SNOWFLAKE_RE = /^\d{17,20}$/;
const TIME_RE = /^([01]\d|2[0-3]):([0-5]\d)$/;
const HEX_RE = /^#?([0-9a-f]{6})$/i;
const CUSTOM_EMOJI_RE = /^<a?:\w{2,32}:\d{17,20}>$/;
const UNICODE_EMOJI_RE = /^(?:\p{RGI_Emoji}|\p{Extended_Pictographic}️?)$/v;
const FORBIDDEN_KEYS = new Set(['__proto__', 'constructor', 'prototype']);
// Control characters except tab and newline never belong in a setting.
const CONTROL_RE = /[\u0000-\u0008\u000b-\u001f\u007f]/;

export const isSnowflake = (v) => typeof v === 'string' && SNOWFLAKE_RE.test(v);

export const isPlainObject = (v) => {
  if (v === null || typeof v !== 'object' || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
};

const clone = (v) => (v === undefined ? undefined : structuredClone(v));

/** A validation failure: `path` is the dotted location ('welcomeCardStyle.background', 'tiers.2.action'). */
export class FieldError extends Error {
  constructor(path, message) {
    super(message);
    this.name = 'FieldError';
    this.path = path;
  }
}

const labelOf = (desc, path) => (desc.label ? `"${desc.label}"` : `"${path}"`);
const join = (path, key) => (path ? `${path}.${key}` : String(key));

/**
 * Ids a value refers to, collected while validating, so they can be checked against the server in one go.
 * @typedef {{ channels: { id: string, path: string, kinds: string[] }[],
 *             roles: { id: string, path: string, assignable: boolean, allowEveryone: boolean, allowDangerous: boolean }[] }} Refs
 */
export const emptyRefs = () => ({ channels: [], roles: [] });

function checkChannel(desc, id, path, refs) {
  if (!isSnowflake(id)) throw new FieldError(path, `${labelOf(desc, path)} must be a channel of this server.`);
  refs.channels.push({ id, path, kinds: [...(desc.channelTypes ?? Object.keys(CHANNEL_KINDS))] });
}

function checkRole(desc, id, path, refs) {
  if (!isSnowflake(id)) throw new FieldError(path, `${labelOf(desc, path)} must be a role of this server.`);
  refs.roles.push({
    id, path, assignable: desc.assignable === true, allowEveryone: desc.allowEveryone === true, allowDangerous: desc.allowDangerous === true,
  });
}

function checkList(desc, value, path) {
  if (!Array.isArray(value)) throw new FieldError(path, `${labelOf(desc, path)} must be a list.`);
  const max = desc.maxItems ?? desc.options?.length ?? 100;
  if (value.length > max) throw new FieldError(path, `${labelOf(desc, path)} can hold at most ${max} entries.`);
  if (desc.minItems && value.length < desc.minItems) {
    throw new FieldError(path, `${labelOf(desc, path)} needs at least ${desc.minItems} entr${desc.minItems === 1 ? 'y' : 'ies'}.`);
  }
}

function uniqueIds(desc, value, path, check, refs) {
  checkList(desc, value, path);
  const seen = new Set();
  value.forEach((id, i) => {
    check(desc, id, join(path, i), refs);
    seen.add(id);
  });
  return [...seen];
}

function validTimezone(tz) {
  if (typeof tz !== 'string' || tz.length === 0 || tz.length > 64) return false;
  if (tz === 'UTC') return true;
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: tz });
    return tz.includes('/');
  } catch {
    return false;
  }
}

function parseColorValue(v) {
  if (Number.isInteger(v) && v >= 0 && v <= 0xffffff) return v;
  if (typeof v === 'string') {
    const m = HEX_RE.exec(v.trim());
    if (m) return parseInt(m[1], 16);
  }
  return null;
}

const toHex = (n) => `#${n.toString(16).padStart(6, '0').toUpperCase()}`;

/**
 * Validates one value against its descriptor and returns the value to store.
 * @param {object} desc a descriptor (see FIELD_TYPES)
 * @param {unknown} value parsed JSON from the browser
 * @param {string} [path] where the value sits, for error messages
 * @param {Refs} [refs] receives every channel and role id met
 * @returns {unknown} the normalised value
 * @throws {FieldError}
 */
export function validateValue(desc, value, path = desc.field ?? '', refs = emptyRefs()) {
  if (value === null && desc.nullable) return null;
  const name = labelOf(desc, path);
  if (value === null && desc.type === 'text') throw new FieldError(path, `${name} cannot be empty.`);
  switch (desc.type) {
    case 'boolean':
      if (typeof value !== 'boolean') throw new FieldError(path, `${name} must be on or off.`);
      return value;

    case 'int':
    case 'float': {
      const ok = desc.type === 'int' ? Number.isInteger(value) : typeof value === 'number' && Number.isFinite(value);
      if (!ok) throw new FieldError(path, `${name} must be ${desc.type === 'int' ? 'a whole number' : 'a number'}.`);
      if (value < desc.min || value > desc.max) throw new FieldError(path, `${name} must be between ${desc.min} and ${desc.max}.`);
      return value;
    }

    case 'text': {
      if (typeof value !== 'string') throw new FieldError(path, `${name} must be text.`);
      if (CONTROL_RE.test(value)) throw new FieldError(path, `${name} contains characters that are not allowed.`);
      const text = desc.multiline ? value.replace(/\r\n?/g, '\n') : value.trim();
      if (desc.nullable && text.trim() === '') return null;
      if (!desc.multiline && /\n/.test(text)) throw new FieldError(path, `${name} must be a single line.`);
      if (text.length > desc.maxLength) throw new FieldError(path, `${name} can be at most ${desc.maxLength} characters.`);
      if ((desc.minLength ?? 0) > 0 && text.trim().length < desc.minLength) {
        throw new FieldError(path, desc.minLength === 1 ? `${name} cannot be empty.` : `${name} needs at least ${desc.minLength} characters.`);
      }
      if (desc.pattern && !new RegExp(`^(?:${desc.pattern})$`, 'u').test(text)) {
        throw new FieldError(path, desc.patternMessage ?? `${name} has an invalid format.`);
      }
      return text;
    }

    case 'enum':
      if (!desc.options.some((o) => o.value === value)) throw new FieldError(path, `${name} must be one of the listed choices.`);
      return value;

    case 'multiEnum': {
      checkList(desc, value, path);
      const allowed = new Set(desc.options.map((o) => o.value));
      for (const [i, v] of value.entries()) {
        if (!allowed.has(v)) throw new FieldError(join(path, i), `${name} contains an unknown choice.`);
      }
      return [...new Set(value)];
    }

    case 'color': {
      const n = parseColorValue(value);
      if (n === null) throw new FieldError(path, `${name} must be a colour like #5865F2.`);
      if (desc.min !== undefined && n < desc.min) throw new FieldError(path, `${name} cannot be pure black (#000000); pick #010101 instead.`);
      return desc.format === 'hex' ? toHex(n) : n;
    }

    case 'timezone':
      if (!validTimezone(value)) throw new FieldError(path, `${name} must be a time zone such as Europe/Berlin or UTC.`);
      return value;

    case 'channel':
      checkChannel(desc, value, path, refs);
      return value;

    case 'channels':
      return uniqueIds(desc, value, path, checkChannel, refs);

    case 'role':
      checkRole(desc, value, path, refs);
      return value;

    case 'roles':
      return uniqueIds(desc, value, path, checkRole, refs);

    case 'user':
      if (!isSnowflake(value)) throw new FieldError(path, `${name} must be a member of this server (pick one, or paste a user id).`);
      return value;

    case 'template': {
      if (!isPlainObject(value)) throw new FieldError(path, `${name} must be a message.`);
      const tpl = normalizeTemplate(value);
      const { ok, errors } = validateTemplate(tpl, { requireSomething: desc.requireContent !== false });
      if (!ok) throw new FieldError(path, `${name}: ${errors[0]}`);
      return tpl;
    }

    case 'embed': {
      if (!isPlainObject(value)) throw new FieldError(path, `${name} must be an embed.`);
      const tpl = normalizeTemplate({ content: null, embed: value, buttons: [] });
      const { ok, errors } = validateTemplate(tpl, { requireSomething: false });
      if (!ok) throw new FieldError(path, `${name}: ${errors[0]}`);
      return tpl.embed;
    }

    case 'stringList': {
      checkList(desc, value, path);
      const out = [];
      const seen = new Set();
      for (const [i, raw] of value.entries()) {
        const p = join(path, i);
        if (typeof raw !== 'string' || CONTROL_RE.test(raw) || /\n/.test(raw)) throw new FieldError(p, `${name} entries must be single lines of text.`);
        let item = raw.trim();
        if (desc.lowercase) item = item.toLowerCase();
        if (item.length < (desc.itemMinLength ?? 1) || item.length > desc.itemMaxLength) {
          throw new FieldError(p, `Each entry of ${name} must be ${desc.itemMinLength ?? 1} to ${desc.itemMaxLength} characters ("${item.slice(0, 40)}").`);
        }
        if (desc.itemPattern && !new RegExp(`^(?:${desc.itemPattern})$`, 'u').test(item)) {
          throw new FieldError(p, `${name}: "${item.slice(0, 40)}" ${desc.itemPatternMessage ?? 'has an invalid format.'}`);
        }
        const key = desc.lowercase ? item : item.toLowerCase();
        if (seen.has(key)) continue;
        seen.add(key);
        out.push(item);
      }
      return out;
    }

    case 'object':
      return validateObject(desc.fields, value, path, refs, name);

    case 'objectList': {
      checkList(desc, value, path);
      return value.map((item, i) => validateObject(desc.itemFields, item, join(path, i), refs, name));
    }

    case 'datetime': {
      const t = typeof value === 'string' && value.length <= 40 ? Date.parse(value) : NaN;
      if (!Number.isFinite(t)) throw new FieldError(path, `${name} must be a date and time.`);
      return new Date(t).toISOString();
    }

    case 'time':
      if (typeof value !== 'string' || !TIME_RE.test(value)) throw new FieldError(path, `${name} must be a time like 09:30.`);
      return value;

    case 'emoji': {
      const v = typeof value === 'string' ? value.trim() : '';
      if (!(CUSTOM_EMOJI_RE.test(v) || (v.length <= 16 && UNICODE_EMOJI_RE.test(v)))) {
        throw new FieldError(path, `${name} must be one emoji.`);
      }
      return v;
    }

    case 'url': {
      const v = typeof value === 'string' ? value.trim() : '';
      if (desc.nullable && v === '') return null;
      if (!isHttpUrl(v) || v.length > (desc.maxLength ?? 2048)) throw new FieldError(path, `${name} must be an http(s) link.`);
      return v;
    }

    default:
      throw new FieldError(path, `${name} has an unknown type.`);
  }
}

function validateObject(fields, value, path, refs, name) {
  if (!isPlainObject(value)) throw new FieldError(path, `${name} must be an object.`);
  const known = new Map(fields.map((f) => [f.field, f]));
  for (const key of Object.keys(value)) {
    if (FORBIDDEN_KEYS.has(key) || !known.has(key)) throw new FieldError(join(path, key), `"${key.slice(0, 40)}" is not a known option.`);
  }
  const out = {};
  for (const f of fields) {
    const p = join(path, f.field);
    if (Object.hasOwn(value, f.field)) out[f.field] = validateValue(f, value[f.field], p, refs);
    else if (f.required) throw new FieldError(p, `${labelOf(f, p)} is required.`);
    else out[f.field] = clone(f.default ?? null);
  }
  return out;
}

/**
 * Reads a stored value leniently for display: anything malformed reads as the descriptor's default. Channel and
 * role ids are kept even when they no longer exist (the browser marks them as deleted). Never throws.
 * @param {object} desc
 * @param {unknown} raw the stored value (a GuildSettings column, a JSON sub-value or a record column)
 * @returns {unknown}
 */
export function readValue(desc, raw) {
  try {
    if (raw === undefined || (raw === null && !desc.nullable && !['template', 'embed', 'object', 'objectList'].includes(desc.type))) {
      return clone(desc.default ?? null);
    }
    if (raw === null && desc.nullable) return null;
    switch (desc.type) {
      case 'template':
        return raw === null ? clone(desc.default ?? null) : normalizeTemplate(raw);
      case 'embed': {
        if (raw === null) return null;
        return normalizeTemplate({ content: null, embed: raw, buttons: [] }).embed;
      }
      case 'object': {
        const src = isPlainObject(raw) ? raw : {};
        return Object.fromEntries(desc.fields.map((f) => [f.field, readValue(f, src[f.field])]));
      }
      case 'objectList':
        return Array.isArray(raw)
          ? raw.filter(isPlainObject).slice(0, desc.maxItems ?? 100)
            .map((item) => Object.fromEntries(desc.itemFields.map((f) => [f.field, readValue(f, item[f.field])])))
          : clone(desc.default ?? []);
      case 'channels':
      case 'roles':
        return Array.isArray(raw) ? raw.filter(isSnowflake) : clone(desc.default ?? []);
      case 'channel':
      case 'role':
      case 'user':
        return isSnowflake(raw) ? raw : clone(desc.default ?? null);
      case 'color': {
        const n = parseColorValue(raw);
        if (n === null) return clone(desc.default ?? null);
        return desc.format === 'hex' ? toHex(n) : n;
      }
      case 'datetime': {
        const t = raw instanceof Date ? raw.getTime() : Date.parse(raw);
        return Number.isFinite(t) ? new Date(t).toISOString() : clone(desc.default ?? null);
      }
      default:
        return validateValue(desc, raw, desc.field ?? '', emptyRefs());
    }
  } catch {
    return clone(desc.default ?? null);
  }
}

/**
 * The server's channels and roles, as the reference check needs them.
 * @typedef {{ channels: Map<string, { kind: string }>,
 *             roles: Map<string, { everyone: boolean, managed: boolean, assignable: boolean }> }} GuildResources
 */

/**
 * Checks collected ids against the server. Ids that a stored value already held (`keep`) pass even when they are
 * gone, so saving an unrelated change never fails because of an old, deleted channel.
 * @param {Refs} refs
 * @param {GuildResources} resources
 * @param {{ keepChannels?: Set<string>, keepRoles?: Set<string> }} [keep] ids that were already stored
 * @throws {FieldError} the first problem
 */
export function checkRefs(refs, resources, { keepChannels = new Set(), keepRoles = new Set() } = {}) {
  for (const ref of refs.channels) {
    const channel = resources.channels.get(ref.id);
    if (!channel) {
      if (keepChannels.has(ref.id)) continue;
      throw new FieldError(ref.path, 'That channel does not exist in this server any more. Pick another one.');
    }
    if (!ref.kinds.includes(channel.kind)) {
      throw new FieldError(ref.path, `That channel is the wrong kind here (expected ${ref.kinds.join(' or ')}).`);
    }
  }
  for (const ref of refs.roles) {
    const role = resources.roles.get(ref.id);
    if (!role) {
      if (keepRoles.has(ref.id)) continue;
      throw new FieldError(ref.path, 'That role does not exist in this server any more. Pick another one.');
    }
    if (role.everyone && !ref.allowEveryone) throw new FieldError(ref.path, '@everyone cannot be used here.');
    if (ref.assignable && (role.managed || !role.assignable)) {
      throw new FieldError(ref.path, role.managed
        ? 'That role is managed by an integration and cannot be given by the bot.'
        : "That role is above the bot's highest role. Move the bot's role higher in Server Settings → Roles, or pick a lower role.");
    }
  }
}

/** Every channel and role id inside a stored value (for checkRefs' `keep` sets). Never throws. */
export function collectStoredRefs(desc, raw, out = { channels: new Set(), roles: new Set() }) {
  try {
    if (raw === null || raw === undefined) return out;
    switch (desc.type) {
      case 'channel': if (isSnowflake(raw)) out.channels.add(raw); break;
      case 'role': if (isSnowflake(raw)) out.roles.add(raw); break;
      case 'channels': if (Array.isArray(raw)) for (const id of raw) if (isSnowflake(id)) out.channels.add(id); break;
      case 'roles': if (Array.isArray(raw)) for (const id of raw) if (isSnowflake(id)) out.roles.add(id); break;
      case 'object': if (isPlainObject(raw)) for (const f of desc.fields) collectStoredRefs(f, raw[f.field], out); break;
      case 'objectList':
        if (Array.isArray(raw)) for (const item of raw) if (isPlainObject(item)) for (const f of desc.itemFields) collectStoredRefs(f, item[f.field], out);
        break;
      default: break;
    }
  } catch { /* a malformed stored value simply keeps nothing */ }
  return out;
}

/**
 * Every role id of a stored record that sits in an `assignable` field (a role the bot gives or takes), with the path of the field
 * that holds it. Never throws.
 * @param {object[]} fields the collection's field descriptors
 * @param {object} record
 * @returns {{ id: string, path: string }[]}
 */
export function collectAssignableRoles(fields, record) {
  const out = [];
  const walk = (desc, raw, path) => {
    try {
      if (raw === null || raw === undefined) return;
      switch (desc.type) {
        case 'role': if (desc.assignable && isSnowflake(raw)) out.push({ id: raw, path }); break;
        case 'roles':
          if (desc.assignable && Array.isArray(raw)) raw.forEach((id, i) => { if (isSnowflake(id)) out.push({ id, path: `${path}.${i}` }); });
          break;
        case 'object': if (isPlainObject(raw)) for (const f of desc.fields) walk(f, raw[f.field], `${path}.${f.field}`); break;
        case 'objectList':
          if (Array.isArray(raw)) raw.forEach((item, i) => { if (isPlainObject(item)) for (const f of desc.itemFields) walk(f, item[f.field], `${path}.${i}.${f.field}`); });
          break;
        default: break;
      }
    } catch { /* a malformed stored value simply holds nothing */ }
  };
  for (const desc of fields ?? []) walk(desc, record?.[desc.field], desc.field);
  return out;
}

/**
 * The key of an id at a place in a value, with list indexes dropped: refKey('tiers.2.roleAdd', id) equals
 * refKey('tiers.0.roleAdd', id) but not refKey('tiers.0.roleRemove', id).
 * @param {string} path
 * @param {string} id
 * @returns {string}
 */
export const refKey = (path, id) => `${String(path).split('.').filter((seg) => !/^\d+$/.test(seg)).join('.')}|${id}`;

/**
 * The refKeys of every channel and role id inside a stored value: which id sits in which field. A role the stored value
 * already held in the same field may stay when its checks would now fail; the same role moved into another field is
 * checked like a new one. Never throws.
 * @param {object} desc
 * @param {unknown} raw
 * @param {string} [path]
 * @param {Set<string>} [out]
 * @returns {Set<string>}
 */
export function storedRefKeys(desc, raw, path = desc.field, out = new Set()) {
  try {
    if (raw === null || raw === undefined) return out;
    switch (desc.type) {
      case 'channel':
      case 'role':
        if (isSnowflake(raw)) out.add(refKey(path, raw));
        break;
      case 'channels':
      case 'roles':
        if (Array.isArray(raw)) for (const id of raw) if (isSnowflake(id)) out.add(refKey(path, id));
        break;
      case 'object':
        if (isPlainObject(raw)) for (const f of desc.fields) storedRefKeys(f, raw[f.field], `${path}.${f.field}`, out);
        break;
      case 'objectList':
        if (Array.isArray(raw)) {
          for (const item of raw) {
            if (isPlainObject(item)) for (const f of desc.itemFields) storedRefKeys(f, item[f.field], `${path}.${f.field}`, out);
          }
        }
        break;
      default: break;
    }
  } catch { /* a malformed stored value simply keeps nothing */ }
  return out;
}

const SERVER_ONLY_KEYS = new Set(['check', 'read', 'write', 'toStored', 'fromStored']);

/**
 * The JSON-safe copy of a descriptor that is served to the browser: functions and server-only keys removed,
 * nested descriptors converted too, deep-frozen.
 * @param {object} desc
 * @returns {Readonly<object>}
 */
export function publicDescriptor(desc) {
  const out = {};
  for (const [key, value] of Object.entries(desc)) {
    if (SERVER_ONLY_KEYS.has(key) || typeof value === 'function' || value instanceof RegExp) continue;
    if ((key === 'fields' || key === 'itemFields') && Array.isArray(value)) out[key] = Object.freeze(value.map(publicDescriptor));
    else out[key] = deepFreeze(clone(value));
  }
  return Object.freeze(out);
}

function deepFreeze(v) {
  if (v && typeof v === 'object') {
    for (const child of Object.values(v)) deepFreeze(child);
    Object.freeze(v);
  }
  return v;
}

/**
 * Checks a descriptor list at start-up (tests call it too): known types, the keys each type needs, unique field
 * names, defaults that validate. Returns the problems (empty = fine).
 * @param {object[]} fields
 * @param {string} [where]
 * @returns {string[]}
 */
export function lintDescriptors(fields, where = '') {
  const problems = [];
  const seen = new Set();
  for (const d of fields) {
    const at = `${where}${d.field}`;
    if (typeof d.field !== 'string' || !d.field) problems.push(`${where}: a descriptor has no field`);
    if (seen.has(d.field)) problems.push(`${at}: duplicate field`);
    seen.add(d.field);
    if (!FIELD_TYPES.includes(d.type)) { problems.push(`${at}: unknown type ${d.type}`); continue; }
    if (typeof d.label !== 'string' || !d.label) problems.push(`${at}: no label`);
    const need = {
      int: ['min', 'max'], float: ['min', 'max'], text: ['maxLength'], enum: ['options'], multiEnum: ['options'],
      color: ['format'], channel: ['channelTypes'], channels: ['channelTypes', 'maxItems'], roles: ['maxItems'],
      stringList: ['maxItems', 'itemMaxLength'], object: ['fields'], objectList: ['itemFields', 'maxItems'],
    }[d.type] ?? [];
    for (const key of need) if (d[key] === undefined) problems.push(`${at}: ${d.type} needs ${key}`);
    for (const kind of d.channelTypes ?? []) if (!Object.hasOwn(CHANNEL_KINDS, kind)) problems.push(`${at}: unknown channel kind ${kind}`);
    if (d.type === 'object') problems.push(...lintDescriptors(d.fields ?? [], `${at}.`));
    if (d.type === 'objectList') problems.push(...lintDescriptors(d.itemFields ?? [], `${at}[].`));
    if (d.default !== undefined && d.default !== null && !['template', 'embed', 'channel', 'channels', 'role', 'roles', 'user'].includes(d.type)) {
      try {
        validateValue(d, d.default, at);
      } catch (err) {
        problems.push(`${at}: the default does not validate (${err.message})`);
      }
    }
  }
  return problems;
}
