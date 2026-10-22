// /search (pkg-search): AniList (`anime`, `manga`), PokeAPI (`pokemon`) and Urban Dictionary (`urban`) lookups. Reads the
// options and hands over to services/searchService.js, which owns the cache, the rate limiting, the adult-title
// filtering, the `urbanNsfwOnly` age gate (/settings urban) and the replies. AniList's MANGA type also covers light
// novels and one-shots; the Format field says which. Pokémon autocomplete only filters an in-memory species index, so it
// never waits for the network.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  autocompletePokemon, executeAniList, executePokemon, executeUrban,
} from '../../services/searchService.js';
import { respondAutocomplete } from '../../utils/interactionHelpers.js';

export const module = 'search';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

export const data = new SlashCommandBuilder()
  .setName('search')
  .setDescription('Look up an anime, manga, Pokémon or Urban Dictionary entry.')
  .addSubcommand((s) => s.setName('anime')
    .setDescription('Search AniList for an anime: score, episodes, status, genres.')
    .addStringOption((o) => o.setName('query')
      .setDescription('Anime title')
      .setRequired(true).setMinLength(1).setMaxLength(100)))
  .addSubcommand((s) => s.setName('manga')
    .setDescription('Search AniList for a manga: score, chapters, status, genres.')
    .addStringOption((o) => o.setName('query')
      .setDescription('Manga title')
      .setRequired(true).setMinLength(1).setMaxLength(100)))
  .addSubcommand((s) => s.setName('pokemon')
    .setDescription('Look up a Pokémon on PokeAPI: types, abilities, stats.')
    .addStringOption((o) => o.setName('name')
      .setDescription('Name or Pokédex number')
      .setRequired(true).setMinLength(1).setMaxLength(40).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('urban')
    .setDescription('Look up a word or phrase on Urban Dictionary.')
    .addStringOption((o) => o.setName('term')
      .setDescription('Word or phrase to define')
      .setRequired(true).setMinLength(1).setMaxLength(100)));

export async function execute(interaction) {
  switch (interaction.options.getSubcommand()) {
    case 'anime': return executeAniList(interaction, 'ANIME');
    case 'manga': return executeAniList(interaction, 'MANGA');
    case 'pokemon': return executePokemon(interaction);
    case 'urban': return executeUrban(interaction);
    default: return undefined;
  }
}

export async function autocomplete(interaction) {
  // Only /search pokemon name has autocomplete; anything else answers with no suggestions.
  if (interaction.options.getSubcommand(false) !== 'pokemon') return respondAutocomplete(interaction, []);
  return autocompletePokemon(interaction);
}
