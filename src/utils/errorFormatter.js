/**
 * Normalises any thrown value into a plain object suitable for structured logging.
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
