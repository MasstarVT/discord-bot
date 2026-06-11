import { Events } from 'discord.js';
import { routeInteraction } from '../handlers/interactionRouter.js';

export const name = Events.InteractionCreate;
export const once = false;

export async function execute(interaction, client) {
  await routeInteraction(interaction, client);
}
