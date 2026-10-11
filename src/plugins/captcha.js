import { peekSettings } from '../services/settingsService.js';
import {
  onMemberJoin, onMemberLeave, onMemberUpdate, runTimeoutJob,
  handleCaptchaButton, handleCaptchaModal, isRoleNewlyAdded, isBulkTarget,
} from '../services/captchaService.js';

export const plugin = {
  id: 'captcha',
  module: 'captcha',
  hooks: [
    // Join gate. Runs after invites/logging and BEFORE welcome (HOOK_ORDER), so ctx.state.captchaPending reaches welcome.
    { event: 'guildMemberAdd', id: 'captcha',
      filter: (ctx) => { const s = peekSettings(ctx.guildId); return !s || (s.captchaEnabled === true && !!s.captchaRoleId); },
      run: onMemberJoin },
    // Cleanup must run even while the module is off (rows created earlier).
    { event: 'guildMemberRemove', id: 'captcha', always: true,
      filter: (ctx) => { const s = peekSettings(ctx.guildId); return !s || s.captchaEnabled === true || !!s.captchaRoleId; },
      run: onMemberLeave },
    // The verified role given by someone else verifies a pending member.
    { event: 'guildMemberUpdate', id: 'captcha',
      filter: (ctx) => {
        const s = peekSettings(ctx.guildId);
        if (s && (!s.captchaEnabled || !s.captchaRoleId)) return false;
        if (s && !isRoleNewlyAdded({ oldMember: ctx.oldMember, newMember: ctx.newMember, oldKnown: ctx.oldKnown, roleId: s.captchaRoleId })) return false;
        return !isBulkTarget(ctx.guildId, ctx.newMember.id);
      },
      run: onMemberUpdate },
  ],
  components: [
    // captcha:start | captcha:answer — gated by the module through the pluginLoader wrapper.
    { type: 'button', prefix: 'captcha', handler: handleCaptchaButton },
    // captcha-modal:answer (checks the module itself) | captcha-modal:panel (admin form; must work while the module is off).
    { type: 'modal', prefix: 'captcha-modal', module: null, handler: handleCaptchaModal },
  ],
  jobs: [{ type: 'captcha.timeout', handler: runTimeoutJob }],
};
