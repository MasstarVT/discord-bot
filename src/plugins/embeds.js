// Embed Messages plugin (pkg-embeds): the persistent `embed-modal` form handler and the messageDelete hook that
// turns an embed whose published message was deleted back into a draft. Thin wrappers over embedService.js.
import { handleEmbedModalSubmit, onEmbedMessageDelete, isEmbedDeleteCandidate } from '../services/embedService.js';

export const plugin = {
  id: 'embeds',
  module: 'embeds',
  hooks: [
    {
      event: 'messageDelete',
      id: 'embeds',
      filter: isEmbedDeleteCandidate, // sync, cheap
      run: onEmbedMessageDelete,
      always: true, // clear stale messageIds even while the module is off
      timeoutMs: 5_000,
    },
  ],
  components: [
    // Gated by plugin.module (installPlugins wraps the handler): a form submitted while the module is off is refused.
    { type: 'modal', prefix: 'embed-modal', handler: handleEmbedModalSubmit },
  ],
  jobs: [],
  provides: {},
};
