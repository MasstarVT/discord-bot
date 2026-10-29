// The channel rule of the slash commands, for the web dashboard's record stores (saved embeds, custom commands, automations):
//   access.channels   { checks: [{ channelId, needs?: PermissionFlagsBits keys, optional?: boolean }] }
// A configuration must not make the bot post in, edit or delete in a channel its author cannot use. The dashboard only knows
// the admin's guild-level permissions (their OAuth entry), which says nothing about a channel's overwrites, so the stores ask
// the bot: here the admin is a real GuildMember and Discord's own overwrite resolution decides (memberChannelProblem, the check
// /embed, /tag and /automation make; the guild owner and Administrators are exempt). Every check is run; the answer's
// `data.problems` lists `{ index, text }` for each one that failed (none: the admin may use all of them). `optional`: a channel
// the bot cannot find is skipped (deleted since; the write that follows handles that), otherwise it is a problem. The bot owner
// acts as the server owner everywhere and is not checked; any other admin who left the server is refused.
import { memberChannelProblem } from '../../utils/privacyGuards.js';
import { TaskError } from './shared.js';

const SNOWFLAKE_RE = /^\d{17,20}$/;
/** What a check may ask for: the needs of /embed, /tag and the automation actions (EXECUTOR_CHANNEL_NEEDS). */
export const ACCESS_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'CreatePublicThreads']);
export const ACCESS_CHECKS_MAX = 25;
const INVALID = 'Those channel checks were not valid. Reload the page and try again.';

/**
 * Re-validates an `access.channels` payload.
 * @returns {{ channelId: string, needs: string[]|undefined, optional: boolean }[]}
 * @throws {TaskError}
 */
export function readAccessPayload(payload) {
  const list = payload?.checks;
  if (!Array.isArray(list) || list.length === 0 || list.length > ACCESS_CHECKS_MAX) throw new TaskError(INVALID);
  return list.map((c) => {
    const needs = c?.needs;
    const badNeeds = needs !== undefined && (!Array.isArray(needs) || needs.length === 0 || !needs.every((k) => ACCESS_NEEDS.includes(k)));
    if (typeof c?.channelId !== 'string' || !SNOWFLAKE_RE.test(c.channelId) || badNeeds) throw new TaskError(INVALID);
    return { channelId: c.channelId, needs, optional: c.optional === true };
  });
}

/** A refusal as plain text for a toast (no markdown, channel mentions as names). */
const plain = (text, guild) => String(text).replace(/\*\*/g, '').replace(/<#(\d+)>/g, (m, id) => `#${guild.channels?.cache?.get(id)?.name ?? 'the channel'}`);

async function findChannel(guild, id) {
  return guild.channels?.cache?.get(id) ?? await guild.channels?.fetch?.(id).catch(() => null) ?? null;
}

export const tasks = {
  'access.channels': {
    async run({ guild, actor, payload }) {
      const checks = readAccessPayload(payload);
      if (actor.isBotOwner) return { message: 'The bot owner is not checked.', data: { problems: [] } };
      if (!actor.member) throw new TaskError('You need to be a member of this server to do that.');
      const problems = [];
      for (const [index, check] of checks.entries()) {
        const channel = await findChannel(guild, check.channelId);
        if (!channel) {
          if (!check.optional) problems.push({ index, text: 'The bot cannot find that channel, so your access to it cannot be checked.' });
          continue;
        }
        const text = memberChannelProblem(channel, actor.member, check.needs);
        if (text) problems.push({ index, text: plain(text, guild) });
      }
      return { message: problems[0]?.text ?? 'You can use those channels.', data: { problems } };
    },
  },
};
