import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.VoiceStateUpdate;
export const once = false;

export async function execute(oldState, newState, client) {
  const member = newState.member ?? oldState.member;
  if (!member || member.user.bot) return;

  // join / leave / move between channels, or 'state' for mute, deafen, stream and camera changes.
  let change = 'state';
  if (!oldState.channelId) change = 'join';
  else if (!newState.channelId) change = 'leave';
  else if (oldState.channelId !== newState.channelId) change = 'move';

  await runHooks('voiceStateUpdate', { client, guild: newState.guild, oldState, newState, member, change });
}
