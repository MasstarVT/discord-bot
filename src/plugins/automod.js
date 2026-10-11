// Automod plugin (pkg-automod.md §5): glue only. Two hooks run the rules: messageCreate (first in
// HOOK_ORDER, so a deleted message never reaches logging, custom commands, XP or translation) and
// messageUpdate (after logging, so the edit is logged before automod acts on it).
// The runner skips both hooks while automodEnabled is false. No side effects at import.
import { runAutomod } from '../services/automodService.js';

const hasMessage = (ctx) => Boolean(ctx.guild && ctx.message && ctx.message.author && !ctx.message.author.bot);

export const plugin = {
  id: 'automod',
  module: 'automod',
  hooks: [
    {
      event: 'messageCreate',
      id: 'automod',
      filter: hasMessage,
      timeoutMs: 15_000,
      run: (ctx) => runAutomod(ctx, { isEdit: false }),
    },
    {
      event: 'messageUpdate',
      id: 'automod',
      filter: hasMessage,
      timeoutMs: 15_000,
      run: (ctx) => runAutomod(ctx, { isEdit: true }),
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
