// /manga (pkg-search): AniList lookup. Reads the option and hands over to services/searchService.js, which owns the
// cache, the rate limiting, the adult-title filtering and the replies. AniList's MANGA type also covers light novels
// and one-shots; the Format field says which.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { executeAniList } from '../../services/searchService.js';

export const module = 'search';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

export const data = new SlashCommandBuilder()
  .setName('manga')
  .setDescription('Search AniList for a manga: score, chapters, status, genres.')
  .addStringOption((o) => o.setName('query')
    .setDescription('Manga title')
    .setRequired(true).setMinLength(1).setMaxLength(100));

export async function execute(interaction) {
  return executeAniList(interaction, 'MANGA');
}
