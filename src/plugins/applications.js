// Applications plugin declaration: the persistent buttons (Apply, Continue, Accept, Deny) and modals (question pages, deny
// reason), and the cleanup hooks. Declaration only: the logic lives in services/applicationService.js and nothing runs at
// import.
import {
  handleButton, handleModal, onChannelDelete, onMessageDelete, onRoleDelete,
} from '../services/applicationService.js';

// Panels and review posts are bot messages; a partial (uncached) message has no author and is looked up by id.
const isOwnOrUnknown = (ctx) => ctx.message?.partial === true || ctx.message?.author?.id === ctx.client?.user?.id;

export const plugin = {
  id: 'applications',
  module: 'applications',
  hooks: [
    // All three are `always`: forgetting a deleted panel, review post, channel or role is cleanup and must happen while the
    // module is off too, otherwise forms would keep pointing at things that are gone.
    { event: 'messageDelete', id: 'applications', always: true, filter: isOwnOrUnknown, run: onMessageDelete },
    { event: 'channelDelete', id: 'applications', always: true, filter: (ctx) => ctx.channel?.isTextBased?.() === true, run: onChannelDelete },
    { event: 'roleDelete', id: 'applications', always: true, run: onRoleDelete },
  ],
  components: [
    // Gated by the foundation wrapper (plugin.module = 'applications'): while the module is off the buttons and forms answer
    // "The Applications module is turned off on this server." and nothing is recorded.
    { type: 'button', prefix: 'application', handler: handleButton },
    { type: 'modal', prefix: 'application-modal', handler: handleModal },
  ],
  jobs: [],
  provides: {},
};
