// Plugin declaration for the games package: registers the persistent Scrabble rack button at startup,
// so a board posted before a restart still answers ("no longer active") instead of failing.
// No init: the dictionary is loaded lazily by /scrabble, so shards that never play Scrabble do not pay
// for it. The collector prefixes (ttt, ttt-ch, rps, scrabble-ch) are not registered here; the router
// answers an unhandled click on them with "This menu has expired." after its grace period.
import { handleRackButton } from '../services/scrabbleService.js';

export const plugin = {
  id: 'games-fixes',
  module: 'games', // the wrapper answers "The Games module is turned off…" when it is disabled
  hooks: [],
  components: [
    { type: 'button', prefix: 'scrabble-rack', handler: handleRackButton },
  ],
  jobs: [],
  provides: {},
};
