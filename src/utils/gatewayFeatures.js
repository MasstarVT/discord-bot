// Gateway features the operator can switch off in .env. Read on every call (cheap), so tests can flip them.

/**
 * False when DISABLE_PRESENCES=1: bot.js then connects without the privileged Presence intent, so every presence cache
 * stays empty. Stat channels fall back to Discord's approximate online count; the live role cannot work.
 */
export function presencesEnabled() {
  return !['1', 'true', 'yes'].includes(String(process.env.DISABLE_PRESENCES ?? '').trim().toLowerCase());
}
