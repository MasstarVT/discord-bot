// Custom commands plugin (pkg-custom-commands.md §5): prefix invocation (`!name args`) and the
// trigger (auto-responder) hook. Declaration only: no logic and no side effects at import.
// ctx.state.customCommand = { tagId, name } is set by the first hook whenever the message invoked an
// existing enabled command (whether or not it then ran); the trigger hook skips such messages, so a
// message gets at most one answer from this package.
import { isPossibleInvocation, handlePrefixInvocation, hasTriggerContent, handleTriggers } from '../services/tagService.js';

export const plugin = {
  id: 'custom-commands',
  module: 'customcommands', // both hooks are skipped by the runner unless customCommandsEnabled === true
  hooks: [
    {
      event: 'messageCreate',
      id: 'custom-commands', // HOOK_ORDER.messageCreate[2]; calls ctx.stop() only after deleting the usage
      filter: (ctx) => isPossibleInvocation(ctx), // SYNC: peekSettings prefix check, no I/O
      run: (ctx) => handlePrefixInvocation(ctx),
      timeoutMs: 15_000, // several REST calls (reply, roles, send, delete)
    },
    {
      event: 'messageCreate',
      id: 'custom-commands:triggers', // HOOK_ORDER.messageCreate[6]; never calls ctx.stop()
      filter: (ctx) => hasTriggerContent(ctx), // SYNC: non-empty content, no custom command ran, triggers not switched off
      run: (ctx) => handleTriggers(ctx),
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
