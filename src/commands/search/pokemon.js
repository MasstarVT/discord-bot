// /pokemon (pkg-search): PokeAPI lookup. Reads the option and hands over to services/searchService.js. Autocomplete only
// filters an in-memory species index, so it never waits for the network.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { executePokemon, autocompletePokemon } from '../../services/searchService.js';

export const module = 'search';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

export const data = new SlashCommandBuilder()
  .setName('pokemon')
  .setDescription('Look up a Pokémon on PokeAPI: types, abilities, stats.')
  .addStringOption((o) => o.setName('name')
    .setDescription('Name or Pokédex number')
    .setRequired(true).setMinLength(1).setMaxLength(40).setAutocomplete(true));

export async function execute(interaction) {
  return executePokemon(interaction);
}

export async function autocomplete(interaction) {
  return autocompletePokemon(interaction);
}
