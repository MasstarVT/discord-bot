// The slash commands' channel rule for the dashboard's record stores: whoever configures a bot post (a saved embed, a custom
// command, an automation) must be able to use the channel themselves (View Channel, plus Send Messages where the bot posts), so
// a Manage Server admin cannot reach a channel that a permission overwrite hides from them. The dashboard only knows the admin's
// guild-level permissions, so the check is a bot task (access.channels, src/services/dashboardTasks/channelAccess.js) that
// resolves the admin as a real GuildMember. The guild owner, the bot owner (both `owner` in `ctx.member`) and Administrators
// are exempt without asking the bot. Anyone else is refused when the check cannot be made (no bot, no answer in time).
import { PermissionFlagsBits } from 'discord.js';
import { RecordError, taskOutcome } from './errors.js';

/** The needs of a post (memberChannelProblem's default) and of looking at a message the bot posted. */
export const SEND_NEEDS = Object.freeze(['ViewChannel', 'SendMessages']);
export const VIEW_NEEDS = Object.freeze(['ViewChannel']);

/** Whether `member` (ctx.member: the OAuth guild entry `{ owner, permissions }`) is the owner or an Administrator. */
export function isChannelExempt(member) {
  if (member?.owner === true) return true;
  try {
    return (BigInt(member?.permissions ?? 0) & PermissionFlagsBits.Administrator) === PermissionFlagsBits.Administrator;
  } catch {
    return false;
  }
}

/**
 * Refuses the write when the dashboard user cannot use a channel the way the write will: the first failed check throws.
 * @param {{ member?: object|null, runTask?: Function }} ctx the store context
 * @param {{ channelId: string|null|undefined, needs?: readonly string[], path?: string, optional?: boolean }[]} checks
 *   `needs`: PermissionFlagsBits keys (default View Channel and Send Messages); `path`: the field the refusal is about;
 *   `optional`: a channel the bot cannot find passes (a stored channel that was deleted)
 * @param {{ lead?: string }} [opts] lead: text put before the bot's refusal (why that channel matters to this write)
 * @throws {RecordError} invalid (a channel the user cannot use), failed (the bot refused, e.g. the user left the server) or
 *   unavailable (the check could not be made: nothing is written)
 */
export async function requireChannelAccess(ctx, checks, { lead = '' } = {}) {
  const wanted = checks.filter((c) => c?.channelId);
  if (wanted.length === 0 || isChannelExempt(ctx?.member)) return;
  if (typeof ctx?.runTask !== 'function') {
    throw new RecordError('unavailable', 'The bot cannot be reached from the dashboard right now, so your access to the channel cannot be checked.');
  }
  const out = taskOutcome(await ctx.runTask('access.channels', {
    checks: wanted.map((c) => ({ channelId: c.channelId, ...(c.needs ? { needs: [...c.needs] } : {}), ...(c.optional ? { optional: true } : {}) })),
  }));
  const problems = out.status === 'done' && Array.isArray(out.data?.problems) ? out.data.problems : null;
  if (!problems) {
    throw new RecordError('unavailable', 'The bot could not check your access to the channel in time. Nothing was changed; try again in a moment.');
  }
  const first = problems[0];
  if (!first) return;
  const path = wanted[first.index]?.path;
  throw new RecordError('invalid', `${lead}${String(first.text ?? 'You cannot use that channel.')}`.slice(0, 300), path ? { path } : {});
}
