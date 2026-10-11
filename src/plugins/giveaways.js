// Giveaways plugin declaration (pkg-giveaways.md §5): the messageDelete / messageDeleteBulk hooks that draft a giveaway whose
// message was deleted, the persistent Participate button and the `giveaway.end` job. Declaration only: the logic lives in
// services/giveawayService.js and nothing runs at import.
import {
  handleEndJob, handleGiveawayBulkDelete, handleGiveawayMessageDelete, handleJoinButton,
} from '../services/giveawayService.js';

export const plugin = {
  id: 'giveaways',
  module: 'giveaways',
  hooks: [
    {
      event: 'messageDelete',
      id: 'giveaways',
      // D8: drafts are tracked even while the module is off.
      always: true,
      // Giveaway messages are bot-authored; a partial (uncached) message has no author and is looked up by id.
      filter: (ctx) => !ctx.message?.author || ctx.message.author.id === ctx.client?.user?.id,
      run: (ctx) => handleGiveawayMessageDelete(ctx),
      timeoutMs: 10_000,
    },
    {
      // A purge fires no messageDelete: one batched lookup drafts every giveaway among the deleted messages.
      event: 'messageDeleteBulk',
      id: 'giveaways',
      always: true,
      run: (ctx) => handleGiveawayBulkDelete(ctx),
      timeoutMs: 10_000,
    },
  ],
  components: [
    // Module gating is applied by installPlugins (plugin.module = 'giveaways').
    { type: 'button', prefix: 'giveaway', handler: (interaction, client) => handleJoinButton(interaction, client) },
  ],
  jobs: [
    { type: 'giveaway.end', handler: (job, ctx) => handleEndJob(job, ctx) },
  ],
  provides: {},
};
