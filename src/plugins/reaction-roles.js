// Reaction-roles plugin declaration (pkg-reaction-roles.md section 5). Declaration only: the hook bodies, filters and the
// persistent component handlers live in services/reactionRoleService.js and nothing runs at import.
import * as rr from '../services/reactionRoleService.js';

// Every hook id is exactly 'reaction-roles' (HOOK_ORDER lists it for each event below; the "event:id" pairs in packages.json are
// not hook ids). All hooks are registered unconditionally because every slot is granted.
//
// Gating:
// - The reaction hooks are gated by the module through the runner: while Reaction Roles is off, reactions do nothing (roles are
//   unchanged and the reactions stay).
// - `messageDelete` and `roleDelete` are `always`: while the module is off, a deleted panel message is still flagged and a deleted
//   role is still pruned from its panels.
// - The `rr` and `rr-select` handlers are wrapped with the module gate by installPlugins, so members get the foundation's
//   "module is turned off" notice and our handlers never run.
//
// The reaction hooks return immediately and work in the background, so starboard, achievements and automations (which run after
// this package in the same dispatch) are never held up.
export const plugin = {
  id: 'reaction-roles',
  module: 'reactionroles',
  hooks: [
    { event: 'messageReactionAdd',         id: 'reaction-roles', filter: rr.reactionHookFilter, run: rr.onReactionAdd },
    { event: 'messageReactionRemove',      id: 'reaction-roles', filter: rr.reactionHookFilter, run: rr.onReactionRemove },
    { event: 'messageReactionRemoveAll',   id: 'reaction-roles', filter: rr.reactionHookFilter, run: rr.onReactionRemoveAll },
    { event: 'messageReactionRemoveEmoji', id: 'reaction-roles', filter: rr.reactionHookFilter, run: rr.onReactionRemoveEmoji },
    { event: 'messageDelete',              id: 'reaction-roles', filter: rr.messageDeleteFilter, run: rr.onMessageDelete, always: true },
    { event: 'roleDelete',                 id: 'reaction-roles', run: rr.onRoleDelete, always: true },
  ],
  components: [
    { type: 'button', prefix: 'rr',        handler: rr.handleRoleButton },
    { type: 'select', prefix: 'rr-select', handler: rr.handleRoleSelect },
  ],
  jobs: [],
  provides: {},
};
