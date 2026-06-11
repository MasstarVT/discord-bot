import { SlashCommandBuilder, AttachmentBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS, xpForLevel } from '../../config/constants.js';
import { errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { levelFromXp, xpIntoLevel } from '../../services/xpService.js';
import { generateProfileCard } from '../../utils/profileCard.js';
import prisma from '../../database/client.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('rank')
  .setDescription('View your rank card or another member\'s.')
  .addUserOption((o) =>
    o.setName('user').setDescription('Member to look up (defaults to you)')
  );

export async function execute(interaction, client) {
  await interaction.deferReply();

  const target = interaction.options.getUser('user') ?? interaction.user;
  const guild  = interaction.guild;
  const guildId = guild.id;

  // Fetch UserLevel record
  const record = await prisma.userLevel.findUnique({
    where: { guildId_userId: { guildId, userId: target.id } },
  });

  if (!record) {
    return interaction.editReply({
      embeds: [infoEmbed('No Data', `${target.username} hasn't earned any XP in this server yet.`)],
    });
  }

  // Compute rank (number of users with strictly more XP + 1)
  const rank = await prisma.userLevel.count({
    where: { guildId, xp: { gt: record.xp } },
  }) + 1;

  const level      = levelFromXp(record.xp);
  const xpCurrent  = xpIntoLevel(record.xp);
  const xpNeeded   = xpForLevel(level);

  // Fetch avatar
  let avatarBuffer = null;
  try {
    const avatarURL = target.displayAvatarURL({ extension: 'png', size: 256 });
    const res = await fetch(avatarURL);
    if (res.ok) avatarBuffer = Buffer.from(await res.arrayBuffer());
  } catch { /* generate card without avatar */ }

  // Generate card
  let cardBuffer;
  try {
    cardBuffer = await generateProfileCard({
      avatarBuffer,
      username:    target.username,
      displayName: target.displayName ?? target.username,
      level,
      totalXp: record.xp,
      rank,
    });
  } catch (err) {
    // Fallback to embed-only
    return interaction.editReply({
      embeds: [{
        color:  COLORS.INFO,
        title:  `${target.displayName ?? target.username}'s Rank`,
        fields: [
          { name: 'Level',    value: String(level),              inline: true },
          { name: 'Rank',     value: `#${rank}`,                 inline: true },
          { name: 'Total XP', value: record.xp.toLocaleString(), inline: true },
          { name: 'Progress', value: `${xpCurrent.toLocaleString()} / ${xpNeeded.toLocaleString()} XP`, inline: true },
          { name: 'Messages', value: record.messageCount.toLocaleString(), inline: true },
          { name: 'Voice',    value: `${record.voiceMinutes} min`, inline: true },
        ],
        thumbnail: { url: target.displayAvatarURL() },
        footer: { text: guild.name },
        timestamp: new Date().toISOString(),
      }],
    });
  }

  const attachment = new AttachmentBuilder(cardBuffer, { name: 'rank.png' });
  await interaction.editReply({ files: [attachment] });
}
