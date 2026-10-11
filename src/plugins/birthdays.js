// Birthdays plugin (pkg-birthdays): the guildReady self-healing pass and the `birthday.tick` job handler. The logic lives in
// services/birthdayService.js.
import { onGuildReady, runBirthdayTick } from '../services/birthdayService.js';

export const plugin = {
  id: 'birthdays',
  module: 'birthdays',
  hooks: [
    // always: the tick keeps running while the module is off. It removes the birthday role, and it keeps recurring so that
    // `/module enable birthdays` (foundation's command, which cannot reschedule our job) takes effect within an hour.
    { event: 'guildReady', id: 'birthdays', always: true, run: onGuildReady },
  ],
  components: [],
  jobs: [{ type: 'birthday.tick', handler: runBirthdayTick }],
  provides: {},
};
