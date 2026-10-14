// Hook point registry (00-overview §8): the order in which plugin hooks run for each event.
// The table is generated from build-plan.mjs (01-foundation §3.3); the runner is src/handlers/hookRegistry.js.
export const HOOK_ORDER = Object.freeze({
  "ready": Object.freeze([]),
  "guildReady": Object.freeze(["invites","moderation","levels","temp-voice","stat-channels","birthdays","feeds","liverole","reminders"]),
  "guildDelete": Object.freeze([]),
  "messageCreate": Object.freeze(["automod","counting","logging","custom-commands","levels","achievements","automations","custom-commands:triggers","translation-fixes"]),
  "messageUpdate": Object.freeze(["logging","automod","automations","starboard"]),
  "messageDelete": Object.freeze(["logging","automations","starboard","giveaways","polls","reaction-roles","embeds","counting"]),
  "messageDeleteBulk": Object.freeze(["logging","starboard","polls","giveaways"]),
  "messageReactionAdd": Object.freeze(["reaction-roles","starboard","achievements","automations"]),
  "messageReactionRemove": Object.freeze(["reaction-roles","starboard"]),
  "messageReactionRemoveAll": Object.freeze(["reaction-roles","starboard"]),
  "messageReactionRemoveEmoji": Object.freeze(["reaction-roles","starboard"]),
  "voiceStateUpdate": Object.freeze(["temp-voice","levels","logging","automations"]),
  "guildMemberAdd": Object.freeze(["invites","moderation","logging","captcha","welcome","automations","stat-channels"]),
  "guildMemberRemove": Object.freeze(["invites","logging","welcome","captcha","stat-channels"]),
  "guildMemberUpdate": Object.freeze(["welcome","captcha","logging","achievements","automations"]),
  "presenceUpdate": Object.freeze(["liverole"]),
  "channelCreate": Object.freeze(["logging"]),
  "channelUpdate": Object.freeze(["logging"]),
  "channelDelete": Object.freeze(["logging","temp-voice","tickets","stat-channels","starboard","feeds","reminders","polls"]),
  "threadCreate": Object.freeze(["logging","automations"]),
  "threadUpdate": Object.freeze(["logging"]),
  "threadDelete": Object.freeze(["logging"]),
  "roleCreate": Object.freeze(["logging"]),
  "roleUpdate": Object.freeze(["logging"]),
  "roleDelete": Object.freeze(["logging","reaction-roles","levels","welcome"]),
  "guildBanAdd": Object.freeze(["moderation","logging"]),
  "guildBanRemove": Object.freeze(["moderation","logging"]),
  "guildUpdate": Object.freeze(["logging"]),
  "emojiCreate": Object.freeze(["logging"]),
  "emojiUpdate": Object.freeze(["logging"]),
  "emojiDelete": Object.freeze(["logging"]),
  "inviteCreate": Object.freeze(["invites","logging"]),
  "inviteDelete": Object.freeze(["invites","logging"]),
  "userUpdate": Object.freeze(["logging"]),
  "levelUp": Object.freeze(["achievements"]),
  "economyPurchase": Object.freeze(["achievements"]),
});

// Internal hook points are emitted by services (rewards, economy), not by a gateway event file.
export const INTERNAL_HOOK_POINTS = Object.freeze(['levelUp', 'economyPurchase']);

// Synthetic hook points are emitted by runGuildReady (01-foundation §8.3), not by an event file.
export const SYNTHETIC_HOOK_POINTS = Object.freeze(['guildReady']);

/** @returns {number} index of `id` in the order for `event`, or -1 when unknown */
export function hookIndex(event, id) {
  const order = Object.hasOwn(HOOK_ORDER, event) ? HOOK_ORDER[event] : null;
  return order ? order.indexOf(id) : -1;
}
