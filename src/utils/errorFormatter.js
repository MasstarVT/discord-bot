/**
 * Normalises any thrown value into a plain object suitable for structured logging.
 *
 * Pure and never throws.
 * - For an `Error` the result is `{ message, name, stack, code, ...context }` (`code` is `null`
 *   when the error has none).
 * - For anything else the result is `{ message: String(err), ...context }`.
 * The context is spread last, so a context key overrides a field of the same name.
 *
 * @param {unknown} err
 * @param {object} [context] - Extra key/value pairs to include (guildId, userId, etc.)
 * @returns {object}
 */
export function formatError(err, context = {}) {
  if (err instanceof Error) {
    return {
      message: err.message,
      name:    err.name,
      stack:   err.stack,
      code:    err.code ?? null,
      ...context,
    };
  }
  return { message: String(err), ...context };
}
