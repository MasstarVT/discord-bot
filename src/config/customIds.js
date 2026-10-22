// customId prefix registry (00-overview §9). Button, select and modal customIds are
// `prefix:param1:param2`; the router splits on ':' and routes by prefix.
// The table is generated from build-plan.mjs (01-foundation §3.4).
export const CUSTOM_ID_PREFIXES = Object.freeze({
  "rr": Object.freeze({ owner: "reaction-roles", types: ["button"], kind: "persistent" }),
  "rr-select": Object.freeze({ owner: "reaction-roles", types: ["select"], kind: "persistent" }),
  "paginator": Object.freeze({ owner: "foundation-utils", types: ["button"], kind: "collector" }),
  "ttt": Object.freeze({ owner: "games-fixes", types: ["button"], kind: "collector" }),
  "ttt-ch": Object.freeze({ owner: "games-fixes", types: ["button"], kind: "collector" }),
  "rps": Object.freeze({ owner: "games-fixes", types: ["button"], kind: "collector" }),
  "scrabble-rack": Object.freeze({ owner: "games-fixes", types: ["button"], kind: "persistent" }),
  "scrabble-ch": Object.freeze({ owner: "games-fixes", types: ["button"], kind: "collector" }),
  "bj": Object.freeze({ owner: "economy-casino", types: ["button"], kind: "collector" }),
  "welcome-setup": Object.freeze({ owner: "welcome", types: ["modal"], kind: "persistent" }),
  "welcome-title": Object.freeze({ owner: "welcome", types: ["field"], kind: "field" }),
  "welcome-description": Object.freeze({ owner: "welcome", types: ["field"], kind: "field" }),
  "welcome-color": Object.freeze({ owner: "welcome", types: ["field"], kind: "field" }),
  "welcome-thumbnail": Object.freeze({ owner: "welcome", types: ["field"], kind: "field" }),
  "welcome-footer": Object.freeze({ owner: "welcome", types: ["field"], kind: "field" }),
  "noop": Object.freeze({ owner: "foundation-runtime", types: ["button"], kind: "persistent" }),
  "confirm": Object.freeze({ owner: "foundation-utils", types: ["button"], kind: "collector" }),
  "tpl-content": Object.freeze({ owner: "foundation-utils", types: ["field"], kind: "field" }),
  "tpl-title": Object.freeze({ owner: "foundation-utils", types: ["field"], kind: "field" }),
  "tpl-description": Object.freeze({ owner: "foundation-utils", types: ["field"], kind: "field" }),
  "tpl-color": Object.freeze({ owner: "foundation-utils", types: ["field"], kind: "field" }),
  "tpl-footer": Object.freeze({ owner: "foundation-utils", types: ["field"], kind: "field" }),
  "welcome-dm-setup": Object.freeze({ owner: "welcome", types: ["modal"], kind: "persistent" }),
  "goodbye-setup": Object.freeze({ owner: "welcome", types: ["modal"], kind: "persistent" }),
  "embed-modal": Object.freeze({ owner: "embeds", types: ["modal"], kind: "persistent" }),
  "reminder-modal": Object.freeze({ owner: "reminders", types: ["modal"], kind: "persistent" }),
  "giveaway": Object.freeze({ owner: "giveaways", types: ["button"], kind: "persistent" }),
  "poll": Object.freeze({ owner: "polls", types: ["button"], kind: "persistent" }),
  "poll-select": Object.freeze({ owner: "polls", types: ["select"], kind: "persistent" }),
  "poll-modal": Object.freeze({ owner: "polls", types: ["modal"], kind: "persistent" }),
  "poll-question": Object.freeze({ owner: "polls", types: ["field"], kind: "field" }),
  "poll-options": Object.freeze({ owner: "polls", types: ["field"], kind: "field" }),
  "ticket": Object.freeze({ owner: "tickets", types: ["button"], kind: "persistent" }),
  "ticket-select": Object.freeze({ owner: "tickets", types: ["select"], kind: "persistent" }),
  "ticket-modal": Object.freeze({ owner: "tickets", types: ["modal"], kind: "persistent" }),
  "ticket-reason": Object.freeze({ owner: "tickets", types: ["field"], kind: "field" }),
  "voice": Object.freeze({ owner: "temp-voice", types: ["button"], kind: "persistent" }),
  "voice-select": Object.freeze({ owner: "temp-voice", types: ["select"], kind: "persistent" }),
  "voice-modal": Object.freeze({ owner: "temp-voice", types: ["modal"], kind: "persistent" }),
  "voice-value": Object.freeze({ owner: "temp-voice", types: ["field"], kind: "field" }),
  "captcha": Object.freeze({ owner: "captcha", types: ["button"], kind: "persistent" }),
  "captcha-modal": Object.freeze({ owner: "captcha", types: ["modal"], kind: "persistent" }),
  "captcha-answer": Object.freeze({ owner: "captcha", types: ["field"], kind: "field" }),
  "automation": Object.freeze({ owner: "automations", types: ["button"], kind: "persistent" }),
  "suggestion": Object.freeze({ owner: "suggestions", types: ["button"], kind: "persistent" }),
  "suggestion-modal": Object.freeze({ owner: "suggestions", types: ["modal"], kind: "persistent" }),
  "suggestion-text": Object.freeze({ owner: "suggestions", types: ["field"], kind: "field" }),
  "modmail": Object.freeze({ owner: "modmail", types: ["button"], kind: "persistent" }),
  "modmail-select": Object.freeze({ owner: "modmail", types: ["select"], kind: "persistent" }),
  "application": Object.freeze({ owner: "applications", types: ["button"], kind: "persistent" }),
  "application-modal": Object.freeze({ owner: "applications", types: ["modal"], kind: "persistent" }),
  "application-q1": Object.freeze({ owner: "applications", types: ["field"], kind: "field" }),
  "application-q2": Object.freeze({ owner: "applications", types: ["field"], kind: "field" }),
  "application-q3": Object.freeze({ owner: "applications", types: ["field"], kind: "field" }),
  "application-q4": Object.freeze({ owner: "applications", types: ["field"], kind: "field" }),
  "application-q5": Object.freeze({ owner: "applications", types: ["field"], kind: "field" }),
  "application-note": Object.freeze({ owner: "applications", types: ["field"], kind: "field" }),
  "sticky-modal": Object.freeze({ owner: "sticky", types: ["modal"], kind: "persistent" }),
  "sticky-content": Object.freeze({ owner: "sticky", types: ["field"], kind: "field" }),
  "antiraid": Object.freeze({ owner: "antiraid", types: ["button"], kind: "persistent" }),
  "event": Object.freeze({ owner: "events", types: ["button"], kind: "persistent" }),
  "event-modal": Object.freeze({ owner: "events", types: ["modal"], kind: "persistent" }),
  "event-title": Object.freeze({ owner: "events", types: ["field"], kind: "field" }),
  "event-description": Object.freeze({ owner: "events", types: ["field"], kind: "field" }),
  "event-start": Object.freeze({ owner: "events", types: ["field"], kind: "field" }),
  "event-duration": Object.freeze({ owner: "events", types: ["field"], kind: "field" }),
  "event-location": Object.freeze({ owner: "events", types: ["field"], kind: "field" }),
  "backup": Object.freeze({ owner: "backups", types: ["button"], kind: "persistent" }),
  "confession": Object.freeze({ owner: "confessions", types: ["button"], kind: "persistent" }),
  "confession-modal": Object.freeze({ owner: "confessions", types: ["modal"], kind: "persistent" }),
  "confession-text": Object.freeze({ owner: "confessions", types: ["field"], kind: "field" }),
  "autothread": Object.freeze({ owner: "auto-threads", types: ["button"], kind: "persistent" }),
});

export const CUSTOM_ID_MAX_LENGTH = 100;

/** @returns {{ prefix: string, params: string[] }} */
export function parseCustomId(customId) {
  const [prefix, ...params] = String(customId ?? '').split(':');
  return { prefix, params };
}

/** @returns {object|null} the registry entry for a prefix, or null */
export function prefixInfo(prefix) {
  return Object.hasOwn(CUSTOM_ID_PREFIXES, prefix) ? CUSTOM_ID_PREFIXES[prefix] : null;
}

/**
 * Builds `prefix:param1:param2`.
 * @throws {Error} on an unknown prefix, params on a `field` prefix, a param containing ':',
 *   or a result longer than 100 characters
 */
export function buildCustomId(prefix, ...params) {
  const info = prefixInfo(prefix);
  if (!info) throw new Error(`Unknown customId prefix "${prefix}"`);
  if (info.kind === 'field' && params.length) throw new Error(`customId prefix "${prefix}" is a field id and takes no params`);
  for (const p of params) {
    if (String(p).includes(':')) throw new Error(`customId param "${p}" contains ':'`);
  }
  const id = [prefix, ...params.map(String)].join(':');
  if (id.length > CUSTOM_ID_MAX_LENGTH) throw new Error(`customId is ${id.length} characters (max ${CUSTOM_ID_MAX_LENGTH})`);
  return id;
}
