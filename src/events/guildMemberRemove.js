import { Events, EmbedBuilder } from 'discord.js';
import { sendLog } from '../services/loggingService.js';

export const name = Events.GuildMemberRemove;
export const once = false;

export async function execute(member, client) {
  const { user, guild } = member;

  // Calculate time spent in server (member may be partial if bot just started)
  const joinedAt   = member.joinedAt;
  const timeInServer = joinedAt
    ? formatTimeInServer(Date.now() - joinedAt.getTime())
    : 'Unknown';

  // Collect roles (excluding @everyone)
  const roles = member.roles?.cache
    .filter((r) => r.id !== guild.id)
    .sort((a, b) => b.position - a.position)
    .map((r) => `<@&${r.id}>`)
    .join(', ')
    .slice(0, 512);

  const embed = new EmbedBuilder()
    .setColor(0xed4245)
    .setTitle('📤 Member Left')
    .setThumbnail(user.displayAvatarURL())
    .addFields(
      { name: 'User',           value: `\`${user.tag}\` <@${user.id}>`,      inline: true },
      { name: 'Time in Server', value: timeInServer,                          inline: true },
    )
    .setFooter({ text: `User ID: ${user.id}` })
    .setTimestamp();

  if (roles) {
    embed.addFields({ name: 'Roles', value: roles });
  }

  await sendLog(guild, 'JOIN_LEAVE', embed, client);

  // ── Leave message ── (enabled in Module C)
  // await sendLeaveMessage(member, client);

  // ── Member cache cleanup ── (enabled in Module C)
  // await clearMemberCache(guild.id, user.id);
}

function formatTimeInServer(ms) {
  const d = Math.floor(ms / 86_400_000);
  const h = Math.floor((ms % 86_400_000) / 3_600_000);
  const m = Math.floor((ms % 3_600_000) / 60_000);
  if (d > 0)  return `${d}d ${h}h`;
  if (h > 0)  return `${h}h ${m}m`;
  if (m > 0)  return `${m}m`;
  return 'Just joined';
}
