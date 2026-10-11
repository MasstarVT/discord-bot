import { peekSettings } from '../services/settingsService.js';
import {
  onMemberJoin, onMemberLeave, onMemberUpdate, onRoleDelete,
  isScreeningCompletion, runAutoroleJob, handleSetupModalSubmit,
} from '../services/welcomeService.js';

export const plugin = {
  id: 'welcome',
  module: 'welcome',
  hooks: [
    // Autoroles run whatever welcomeEnabled says; message/DM sending check the flag themselves.
    { event: 'guildMemberAdd', id: 'welcome', module: null, timeoutMs: 15_000, run: onMemberJoin },
    // Cancels pending autorole jobs always; posts the goodbye only when welcomeEnabled && goodbyeEnabled.
    { event: 'guildMemberRemove', id: 'welcome', module: null,
      filter: (ctx) => ctx.user?.id !== ctx.client?.user?.id, run: onMemberLeave },
    // Membership-screening completion (cached transition, or uncached member: R108-R110).
    { event: 'guildMemberUpdate', id: 'welcome', module: null,
      filter: (ctx) => isScreeningCompletion({ oldMember: ctx.oldMember, newMember: ctx.newMember, oldKnown: ctx.oldKnown }) !== null
        && (peekSettings(ctx.guildId)?.autoroleIds?.length ?? 1) > 0,
      run: onMemberUpdate },
    // Prune deleted roles from autoroleIds (autoroles are not module-gated, so neither is pruning).
    { event: 'roleDelete', id: 'welcome', module: null,
      filter: (ctx) => { const s = peekSettings(ctx.guildId); return !s || (s.autoroleIds ?? []).includes(ctx.role.id); },
      run: onRoleDelete },
  ],
  components: [
    // module: null — the setup forms must work while the module is off (/welcome is gated=false).
    { type: 'modal', prefix: 'welcome-setup',    module: null, handler: (i) => handleSetupModalSubmit('welcome', i) },
    { type: 'modal', prefix: 'welcome-dm-setup', module: null, handler: (i) => handleSetupModalSubmit('dm', i) },
    { type: 'modal', prefix: 'goodbye-setup',    module: null, handler: (i) => handleSetupModalSubmit('goodbye', i) },
  ],
  jobs: [{ type: 'welcome.autorole', handler: runAutoroleJob }],
};
