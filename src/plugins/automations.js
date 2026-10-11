// Automations plugin declaration (pkg-automations.md §5). Declaration only: the hook bodies, filters, the dispatcher and the
// button handler live in services/automationService.js and nothing runs at import.
import * as auto from '../services/automationService.js';

// Every hook id is exactly 'automations' (HOOK_ORDER lists it for each event below).
// Gating: every hook and the button component are gated by the module through the runner / component wrapper. No hook is `always`
// (there is nothing to clean up while the module is off), and there are no scheduler jobs and no reward providers.
export const plugin = {
  id: 'automations',
  module: 'automations',
  hooks: [
    { event: 'messageCreate',      id: 'automations', filter: auto.filterMessageCreate, run: auto.onMessageCreate },
    { event: 'messageUpdate',      id: 'automations', filter: auto.filterMessageUpdate, run: auto.onMessageUpdate },
    { event: 'messageDelete',      id: 'automations', filter: auto.filterMessageDelete, run: auto.onMessageDelete },
    { event: 'messageReactionAdd', id: 'automations', filter: auto.filterReactionAdd,   run: auto.onReactionAdd },
    { event: 'voiceStateUpdate',   id: 'automations', filter: auto.filterVoiceState,    run: auto.onVoiceState },
    { event: 'guildMemberAdd',     id: 'automations', filter: auto.filterMemberAdd,     run: auto.onMemberAdd },
    { event: 'guildMemberUpdate',  id: 'automations', filter: auto.filterMemberUpdate,  run: auto.onMemberUpdate },
    { event: 'threadCreate',       id: 'automations', filter: auto.filterThreadCreate,  run: auto.onThreadCreate },
  ],
  components: [
    { type: 'button', prefix: 'automation', handler: auto.onAutomationButton },
  ],
  jobs: [],
  provides: {},
};
