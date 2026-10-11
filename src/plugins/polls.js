// Polls plugin declaration (pkg-polls.md §5): the delete hooks that end a poll whose message or channel vanished, the
// persistent vote button / select and create modal, and the `poll.end` job. Declaration only: the logic lives in
// services/pollService.js and nothing runs at import.
import {
  handleCreateModal, handleEndJob, handleVoteButton, handleVoteSelect,
  onChannelDelete, onMessageDelete, onMessageDeleteBulk,
} from '../services/pollService.js';

// Poll messages are bot-authored; a partial (uncached) message has no author and is looked up by id.
const isOwnOrUnknown = (ctx) => ctx.message?.partial === true || ctx.message?.author?.id === ctx.client?.user?.id;

export const plugin = {
  id: 'polls',
  module: 'polls',
  hooks: [
    // `always`: ending a poll whose message or channel vanished is cleanup and must happen even while the module is off,
    // otherwise dead polls would keep holding the 50-poll cap.
    { event: 'messageDelete', id: 'polls', always: true, filter: isOwnOrUnknown, run: (ctx) => onMessageDelete(ctx) },
    { event: 'messageDeleteBulk', id: 'polls', always: true, run: (ctx) => onMessageDeleteBulk(ctx) },
    { event: 'channelDelete', id: 'polls', always: true, run: (ctx) => onChannelDelete(ctx) },
  ],
  components: [
    // Module gating is applied by installPlugins (plugin.module = 'polls'): while the module is off the wrapper answers
    // "The Polls module is turned off on this server." and nothing is recorded.
    { type: 'button', prefix: 'poll', handler: (interaction, client) => handleVoteButton(interaction, client) },
    { type: 'select', prefix: 'poll-select', handler: (interaction, client) => handleVoteSelect(interaction, client) },
    { type: 'modal', prefix: 'poll-modal', handler: (interaction, client) => handleCreateModal(interaction, client) },
  ],
  jobs: [{ type: 'poll.end', handler: (job, ctx) => handleEndJob(job, ctx) }],
  provides: {},
};
