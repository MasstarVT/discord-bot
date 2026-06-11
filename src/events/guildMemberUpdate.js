import { Events, EmbedBuilder } from 'discord.js';
import { sendLog } from '../services/loggingService.js';
import { assignAutorole } from '../services/welcomeService.js';

export const name = Events.GuildMemberUpdate;
export const once = false;

export async function execute(oldMember, newMember, client) {
  const guild = newMember.guild;
  const user  = newMember.user;

  // ── Rules Screening completion ─────────────────────────────────────────────
  // When a member completes Discord's member screening, pending changes false.
  // This is when we assign autoroles for guilds that use screening.
  if (oldMember.pending === true && newMember.pending === false) {
    await assignAutorole(newMember, client);
  }

  // ── Collect changes for the audit log ─────────────────────────────────────
  const changes = [];

  if (oldMember.nickname !== newMember.nickname) {
    const before = oldMember.nickname ?? `*${user.username}* (no nickname)`;
    const after  = newMember.nickname ?? `*${user.username}* (no nickname)`;
    changes.push({ name: '📝 Nickname Changed', value: `**Before:** ${before}\n**After:** ${after}` });
  }

  const addedRoles   = newMember.roles.cache.filter((r) => !oldMember.roles.cache.has(r.id) && r.id !== guild.id);
  const removedRoles = oldMember.roles.cache.filter((r) => !newMember.roles.cache.has(r.id) && r.id !== guild.id);

  if (addedRoles.size > 0) {
    changes.push({
      name:  '✅ Roles Added',
      value: addedRoles.map((r) => `<@&${r.id}>`).join(', ').slice(0, 512),
    });
  }
  if (removedRoles.size > 0) {
    changes.push({
      name:  '❌ Roles Removed',
      value: removedRoles.map((r) => `<@&${r.id}>`).join(', ').slice(0, 512),
    });
  }

  const wasTimedOut = oldMember.communicationDisabledUntil !== null;
  const isTimedOut  = newMember.communicationDisabledUntil !== null;
  if (!wasTimedOut && isTimedOut) {
    const until = Math.floor(newMember.communicationDisabledUntil.getTime() / 1000);
    changes.push({ name: '⏱️ Timed Out', value: `Expires <t:${until}:R>` });
  } else if (wasTimedOut && !isTimedOut) {
    changes.push({ name: '🔊 Timeout Removed', value: 'Timeout lifted' });
  }

  if (changes.length === 0) return;

  const embed = new EmbedBuilder()
    .setColor(0x5865f2)
    .setTitle('👤 Member Updated')
    .setAuthor({ name: user.tag, iconURL: user.displayAvatarURL() })
    .addFields(
      { name: 'Member', value: `<@${user.id}> \`${user.tag}\``, inline: true },
      ...changes,
    )
    .setFooter({ text: `User ID: ${user.id}` })
    .setTimestamp();

  await sendLog(guild, 'MEMBER', embed, client);
}
