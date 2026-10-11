// Plugin `translation-fixes`: auto-translate as the last messageCreate hook (after automod and the
// custom-command hooks, so a message automod deleted never reaches it).
import { handleAutoTranslate, translationPaused } from '../services/translationService.js';

export const plugin = {
  id: 'translation-fixes',
  module: 'translation', // the runner skips the hook unless translationEnabled === true
  hooks: [
    {
      event: 'messageCreate',
      id: 'translation-fixes', // HOOK_ORDER.messageCreate: last
      // Sync and cheap: it runs before settings are loaded, for every guild message.
      filter: (ctx) => typeof ctx.message?.content === 'string'
        && ctx.message.content.length > 0
        && !translationPaused(),
      run: (ctx) => handleAutoTranslate(ctx),
      // Longer than AUTO_DEADLINE_MS (25 s) plus one provider request and the reply; the default 10 s is too short for 5 targets.
      timeoutMs: 45_000,
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
