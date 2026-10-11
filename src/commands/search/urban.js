// /urban (pkg-search): Urban Dictionary lookup. Reads the option and hands over to services/searchService.js, which
// refuses it outside age-restricted channels while the guild's urbanNsfwOnly setting is on (the default; /settings urban).
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { executeUrban } from '../../services/searchService.js';

export const module = 'search';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

export const data = new SlashCommandBuilder()
  .setName('urban')
  .setDescription('Look up a word or phrase on Urban Dictionary.')
  .addStringOption((o) => o.setName('term')
    .setDescription('Word or phrase to define')
    .setRequired(true).setMinLength(1).setMaxLength(100));

export async function execute(interaction) {
  return executeUrban(interaction);
}
