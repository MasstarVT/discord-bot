import { Events, EmbedBuilder } from 'discord.js';
import { sendLog }        from '../services/loggingService.js';
import { handleVoiceXp } from '../services/xpService.js';

export const name = Events.VoiceStateUpdate;
export const once = false;

export async function execute(oldState, newState, client) {
  const guild  = newState.guild;
  const member = newState.member;
  if (!member || member.user.bot) return;

  const oldChannel = oldState.channel;
  const newChannel = newState.channel;

  let title, color, description;

  // ── Joins, leaves, or moves ───────────────────────────────────────────────
  if (!oldChannel && newChannel) {
    title       = '🔊 Joined Voice';
    color       = 0x57f287;
    description = `<@${member.id}> joined <#${newChannel.id}>`;
  } else if (oldChannel && !newChannel) {
    title       = '🔇 Left Voice';
    color       = 0xed4245;
    description = `<@${member.id}> left <#${oldChannel.id}>`;
  } else if (oldChannel && newChannel && oldChannel.id !== newChannel.id) {
    title       = '↔️ Moved Voice Channel';
    color       = 0x5865f2;
    description = `<@${member.id}> moved from <#${oldChannel.id}> → <#${newChannel.id}>`;
  } else {
    // State-only changes (mute/deafen/stream) — build change list
    const changes = [];
    if (!oldState.serverMute   && newState.serverMute)   changes.push('Server muted');
    if (oldState.serverMute    && !newState.serverMute)  changes.push('Server unmuted');
    if (!oldState.serverDeaf   && newState.serverDeaf)   changes.push('Server deafened');
    if (oldState.serverDeaf    && !newState.serverDeaf)  changes.push('Server undeafened');
    if (!oldState.selfDeaf     && newState.selfDeaf)     changes.push('Self-deafened');
    if (oldState.selfDeaf      && !newState.selfDeaf)    changes.push('Self-undeafened');
    if (!oldState.selfMute     && newState.selfMute)     changes.push('Self-muted');
    if (oldState.selfMute      && !newState.selfMute)    changes.push('Self-unmuted');
    if (!oldState.streaming    && newState.streaming)    changes.push('Started streaming');
    if (oldState.streaming     && !newState.streaming)   changes.push('Stopped streaming');
    if (changes.length === 0) return;

    title       = '🔔 Voice State Changed';
    color       = 0x99aab5;
    description = `<@${member.id}> in <#${newChannel?.id ?? oldChannel?.id}>\n${changes.join(', ')}`;
  }

  const embed = new EmbedBuilder()
    .setColor(color)
    .setTitle(title)
    .setDescription(description)
    .setAuthor({ name: member.user.tag, iconURL: member.user.displayAvatarURL() })
    .setFooter({ text: `User ID: ${member.id}` })
    .setTimestamp();

  await sendLog(guild, 'VOICE', embed, client);

  // Voice XP tracking (Module E)
  await handleVoiceXp(oldState, newState, client);
}
