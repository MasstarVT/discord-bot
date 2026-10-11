// Achievements plugin declaration (pkg-achievements.md §5): wires the counting hooks to the service. Declaration only:
// no logic and no side effects at import. Every hook id is exactly 'achievements' and is gated by the module through
// the runner; payout and announcements are started fire-and-forget by the service, so later hooks are never held up.
import * as svc from '../services/achievementService.js';

export const plugin = {
  id: 'achievements',
  module: 'achievements',
  hooks: [
    { event: 'messageCreate', id: 'achievements', filter: svc.messageFilter, run: svc.onMessageCreate, timeoutMs: 5_000 },
    { event: 'messageReactionAdd', id: 'achievements', filter: svc.reactionFilter, run: svc.onReactionAdd, timeoutMs: 5_000 },
    { event: 'guildMemberUpdate', id: 'achievements', filter: svc.memberUpdateFilter, run: svc.onMemberUpdate },
    { event: 'levelUp', id: 'achievements', filter: svc.levelUpFilter, run: svc.onLevelUp },
    { event: 'economyPurchase', id: 'achievements', filter: svc.purchaseFilter, run: svc.onPurchase },
  ],
  components: [], // no prefix of our own; only the paginator and confirm buttons through the helpers
  jobs: [],       // deadlines are checked lazily; no job type exists for this package
  provides: {},
};
