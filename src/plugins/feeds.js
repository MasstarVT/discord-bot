// Feeds plugin (pkg-feeds): the guildReady job repair, the channelDelete cleanup and the `feed.poll` job handler.
// The logic lives in services/feedService.js; the /feed command is src/commands/admin/feed.js.
import { FEED_CHANNEL_TYPES, disableFeedsForChannel, ensureGuildFeedJobs, pollFeedJob } from '../services/feedService.js';

export const plugin = {
  id: 'feeds',
  module: 'feeds',
  hooks: [
    {
      event: 'guildReady',
      id: 'feeds', // HOOK_ORDER.guildReady contains 'feeds'
      always: true, // jobs must exist even while the module is off; the handler skips the work
      timeoutMs: 30_000,
      run: (ctx) => ensureGuildFeedJobs(ctx.guild),
    },
    {
      event: 'channelDelete',
      id: 'feeds', // HOOK_ORDER.channelDelete contains 'feeds'
      always: true, // cleanup runs even while the module is off
      filter: (ctx) => FEED_CHANNEL_TYPES.includes(ctx.channel?.type),
      run: (ctx) => disableFeedsForChannel(ctx),
    },
  ],
  components: [], // feeds use no customIds and request no prefix
  jobs: [{ type: 'feed.poll', handler: pollFeedJob }],
  provides: {},
};
