// Bot joins plugin: records who added the bot to each server, which the web dashboard shows the bot owner.
// Declaration only: no logic and no side effects at import; the work is in src/services/botJoinService.js.
import { recordBotJoin } from '../services/botJoinService.js';

export const plugin = {
  id: 'bot-joins',
  module: null,
  hooks: [{ event: 'guildReady', id: 'bot-joins', timeoutMs: 15_000, run: (ctx) => recordBotJoin(ctx) }],
  components: [],
  jobs: [],
  provides: {},
};
