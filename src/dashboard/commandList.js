// The public command reference (/docs): a plain summary of every slash command, built from the command files by
// scripts/build-command-list.js and kept in src/dashboard/commands.json, so the dashboard process never imports
// src/commands (which pulls in the database client). test/integration.test.js checks the file is current.
import { PERMISSION_LEVELS } from '../config/constants.js';

/** Who can run a command, by permissionLevel (a command without one is open to everyone). */
const WHO = Object.freeze({
  [PERMISSION_LEVELS.MEMBER]: 'everyone',
  [PERMISSION_LEVELS.MODERATOR]: 'moderators',
  [PERMISSION_LEVELS.ADMIN]: 'admins',
  [PERMISSION_LEVELS.OWNER]: 'owner',
});

/** Discord option types for subcommands and subcommand groups. */
const SUB_COMMAND = 1;
const SUB_COMMAND_GROUP = 2;

/** The subcommands of a payload, groups flattened to "group sub", in declaration order. */
function subcommands(payload) {
  return (payload.options ?? []).flatMap((opt) => {
    if (opt.type === SUB_COMMAND) return [{ name: opt.name, description: opt.description }];
    if (opt.type === SUB_COMMAND_GROUP) {
      return (opt.options ?? []).map((sub) => ({ name: `${opt.name} ${sub.name}`, description: sub.description }));
    }
    return [];
  });
}

/**
 * Summarises loaded command files, sorted by name.
 * @param {{ ns: { module?: string|null, permissionLevel?: number }, payload: { name: string, description: string, options?: object[] } }[]} entries
 * @returns {{ name: string, description: string, module: string|null, who: string, subcommands: { name: string, description: string }[] }[]}
 */
export function summarizeCommands(entries) {
  return entries
    .map(({ ns, payload }) => ({
      name: payload.name,
      description: payload.description,
      module: ns.module ?? null,
      who: WHO[ns.permissionLevel ?? PERMISSION_LEVELS.MEMBER] ?? 'everyone',
      subcommands: subcommands(payload),
    }))
    .sort((a, b) => a.name.localeCompare(b.name));
}
