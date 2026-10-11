// Economy plugin declaration (pkg-economy.md §7): registers the `coins` provider on the rewards bus, so giveaways,
// achievements and automations can grant and take coins without importing the economy. Declaration only: no logic and no
// side effects at import. 'bj' (blackjack) is a collector prefix and is never registered as a persistent component.
import { grantCoinsReward, takeCoinsReward } from '../services/economyService.js';

export const plugin = {
  id: 'economy',
  module: 'economy',
  hooks: [],
  components: [],
  jobs: [],
  provides: {
    coins: { grant: grantCoinsReward, take: takeCoinsReward },
  },
};
