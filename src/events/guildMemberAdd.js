import { Events, EmbedBuilder } from 'discord.js';
import { sendLog } from '../services/loggingService.js';
import { sendWelcome, assignAutorole } from '../services/welcomeService.js';

export const name = Events.GuildMemberAdd;
export const once = false;

const NEW_ACCOUNT_THRESHOLD_MS = 7 * 24 * 60 * 60 * 1_000;

export async function execute(member, client) {
  const { user, guild } = member;

  // ── Traffic log ────────────────────────────────────────────────────────────
  const accountAgeMs = Date.now() - user.createdTimestamp;
  const isNewAccount = accountAgeMs < NEW_ACCOUNT_THRESHOLD_MS;
  const createdTs    = Math.floor(user.createdTimestamp / 1000);

  const logEmbed = new EmbedBuilder()
    .setColor(isNewAccount ? 0xff9800 : 0x57f287)
    .setTitle(isNewAccount ? '⚠️ New Account Joined' : '📥 Member Joined')
    .setThumbnail(user.displayAvatarURL())
    .addFields(
      { name: 'User',            value: `<@${user.id}> \`${user.tag}\``,   inline: true },
      { name: 'Account Created', value: `<t:${createdTs}:R>`,              inline: true },
      { name: 'Member Count',    value: `#${guild.memberCount.toLocaleString()}`, inline: true },
    )
    .setFooter({ text: `User ID: ${user.id}` })
    .setTimestamp();

  if (isNewAccount) {
    logEmbed.setDescription('⚠️ **This account was created less than 7 days ago.**');
  }

  await sendLog(guild, 'JOIN_LEAVE', logEmbed, client);

  // ── Welcome message + invite tracking ─────────────────────────────────────
  await sendWelcome(member, client);

  // ── Autorole ───────────────────────────────────────────────────────────────
  // assignAutorole skips internally if member.pending === true (Rules Screening).
  // The guildMemberUpdate event handles the pending → false transition.
  await assignAutorole(member, client);
}
