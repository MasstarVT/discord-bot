// The error a collection store throws for an expected failure; the server maps `code` to an HTTP status and shows
// `message` to the admin verbatim. Pure.

/** code -> HTTP status. */
export const RECORD_ERROR_STATUS = Object.freeze({
  invalid: 400, // the input breaks a rule the descriptors cannot express (shown as is)
  not_found: 404, // no such record in this server
  conflict: 409, // a unique name or key is taken
  limit: 409, // the server already has the maximum number of these records
  discord: 502, // Discord refused or failed a write (missing permission, deleted channel, outage)
  unavailable: 503, // the feature cannot be managed from the dashboard right now
});

export class RecordError extends Error {
  /**
   * @param {keyof RECORD_ERROR_STATUS} code
   * @param {string} message shown to the admin
   * @param {{ path?: string }} [extra] path: the field the message is about
   */
  constructor(code, message, extra = {}) {
    super(message);
    this.name = 'RecordError';
    this.code = Object.hasOwn(RECORD_ERROR_STATUS, code) ? code : 'invalid';
    if (extra.path) this.path = extra.path;
  }

  get status() {
    return RECORD_ERROR_STATUS[this.code];
  }
}
