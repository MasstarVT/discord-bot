// Scheduler job registry (00-overview §9). `recurring` jobs re-seed themselves via
// { nextRunAt } and are cancelled when the bot leaves a guild; `dedupe` documents the dedupeKey.
// The table is generated from build-plan.mjs (01-foundation §3.5).
export const JOB_TYPES = Object.freeze({
  "mod.unban": Object.freeze({ owner: "moderation", recurring: false, dedupe: "mod.unban:{guildId}:{userId}" }),
  "mod.expire": Object.freeze({ owner: "moderation", recurring: false, dedupe: "mod.expire:{infractionId}" }),
  "welcome.autorole": Object.freeze({ owner: "welcome", recurring: false, dedupe: "welcome.autorole:{guildId}:{userId}" }),
  "captcha.timeout": Object.freeze({ owner: "captcha", recurring: false, dedupe: "captcha.timeout:{guildId}:{userId}" }),
  "reminder.fire": Object.freeze({ owner: "reminders", recurring: true, dedupe: "reminder:{reminderId}" }),
  "giveaway.end": Object.freeze({ owner: "giveaways", recurring: false, dedupe: "giveaway.end:{giveawayId}" }),
  "poll.end": Object.freeze({ owner: "polls", recurring: false, dedupe: "poll.end:{pollId}" }),
  "tempvoice.cleanup": Object.freeze({ owner: "temp-voice", recurring: false, dedupe: "tempvoice.cleanup:{channelId}" }),
  "stats.refresh": Object.freeze({ owner: "stat-channels", recurring: true, dedupe: "stats.refresh:{guildId}" }),
  "birthday.tick": Object.freeze({ owner: "birthdays", recurring: true, dedupe: "birthday.tick:{guildId}" }),
  "feed.poll": Object.freeze({ owner: "feeds", recurring: true, dedupe: "feed.poll:{feedId}" }),
  "levels.voiceSweep": Object.freeze({ owner: "levels", recurring: true, dedupe: "levels.voiceSweep:{guildId}" }),
  "dashboard.task": Object.freeze({ owner: "dashboard-tasks", recurring: false, dedupe: "dashboard.task:{taskId}", interactive: true }),
  "antiraid.unlock": Object.freeze({ owner: "antiraid", recurring: false, dedupe: "antiraid.unlock:{guildId}" }),
  "event.remind": Object.freeze({ owner: "events", recurring: false, dedupe: "event.remind:{eventId}" }),
  "event.end": Object.freeze({ owner: "events", recurring: false, dedupe: "event.end:{eventId}" }),
  "boosts.sweep": Object.freeze({ owner: "boosts", recurring: true, dedupe: "boosts.sweep:{guildId}" }),
  "backup.auto": Object.freeze({ owner: "backups", recurring: true, dedupe: "backup.auto:{guildId}" }),
  "backup.restore": Object.freeze({ owner: "backups", recurring: false, dedupe: "backup.restore:{guildId}" }),
  "bump.remind": Object.freeze({ owner: "bumps", recurring: false, dedupe: "bump.remind:{guildId}:{site}" }),
  "ticket.managersync": Object.freeze({ owner: "tickets", recurring: false, dedupe: "ticket.managersync:{guildId}" }),
});

/** @returns {boolean} true when `type` is a registered job type */
export function isJobType(type) {
  return typeof type === 'string' && Object.hasOwn(JOB_TYPES, type);
}

// Jobs someone is waiting for (the dashboard's actions): claimed by their own fast loop, and failed rather than run late.
export const INTERACTIVE_JOB_TYPES = Object.freeze(Object.keys(JOB_TYPES).filter((t) => JOB_TYPES[t].interactive === true));

// Used by guildDelete.js to cancel a departed guild's recurring jobs.
export const RECURRING_JOB_TYPES = Object.freeze(Object.keys(JOB_TYPES).filter((t) => JOB_TYPES[t].recurring));
