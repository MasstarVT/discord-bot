import { Events } from 'discord.js';
import { runAutomod }      from '../services/automodService.js';
import { grantXp }         from '../services/xpService.js';
import { runTriggers }     from '../services/tagService.js';
import { runAutoTranslate } from '../services/translationService.js';

export const name = Events.MessageCreate;
export const once = false;

export async function execute(message, client) {
  if (message.author.bot || !message.guild) return;

  await runAutomod(message, client);
  await grantXp(message, client);
  await runTriggers(message, client);
  await runAutoTranslate(message, client);
}
