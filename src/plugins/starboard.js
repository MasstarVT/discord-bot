// Starboard plugin declaration (pkg-starboard.md §5). Declaration only: the hook bodies, filters and the sync engine live in
// services/starboardService.js and nothing runs at import.
import * as sb from '../services/starboardService.js';

// Every hook id is exactly 'starboard' (HOOK_ORDER lists it for each event below).
// Gating: the reaction and edit hooks are gated by the module through the runner. The delete hooks are `always`: while the
// module is off, a deleted source still removes its repost (privacy), a purged repost is still forgotten, and a deleted board
// channel still disables its board.
export const plugin = {
  id: 'starboard',
  module: 'starboard',
  hooks: [
    { event: 'messageReactionAdd',         id: 'starboard', filter: sb.reactionFilter,     run: sb.onReactionEvent },
    { event: 'messageReactionRemove',      id: 'starboard', filter: sb.reactionFilter,     run: sb.onReactionEvent },
    { event: 'messageReactionRemoveAll',   id: 'starboard', filter: sb.reactionFilter,     run: sb.onReactionEvent },
    { event: 'messageReactionRemoveEmoji', id: 'starboard', filter: sb.reactionFilter,     run: sb.onReactionEvent },
    { event: 'messageUpdate',              id: 'starboard', filter: sb.guildMayHaveBoards, run: sb.onMessageUpdate },
    { event: 'messageDelete',              id: 'starboard', filter: sb.guildMayHaveBoards, run: sb.onMessageDelete, always: true },
    { event: 'messageDeleteBulk',          id: 'starboard', filter: sb.guildMayHaveBoards, run: sb.onMessageDeleteBulk, always: true },
    { event: 'channelDelete',              id: 'starboard', filter: sb.guildMayHaveBoards, run: sb.onChannelDelete, always: true },
  ],
  components: [], // the jump button is a Link button with no customId; no prefix needed
  jobs: [],       // no scheduler jobs
  provides: {},
};
