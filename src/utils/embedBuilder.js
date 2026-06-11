import { EmbedBuilder } from 'discord.js';
import { COLORS } from '../config/constants.js';

function base(color) {
  return new EmbedBuilder().setColor(color).setTimestamp();
}

export function successEmbed(title, description, footer) {
  const e = base(COLORS.SUCCESS).setTitle(`✅  ${title}`);
  if (description) e.setDescription(description);
  if (footer) e.setFooter({ text: footer });
  return e;
}

export function errorEmbed(title, description, footer) {
  const e = base(COLORS.ERROR).setTitle(`❌  ${title}`);
  if (description) e.setDescription(description);
  if (footer) e.setFooter({ text: footer });
  return e;
}

export function infoEmbed(title, description, footer) {
  const e = base(COLORS.INFO).setTitle(`ℹ️  ${title}`);
  if (description) e.setDescription(description);
  if (footer) e.setFooter({ text: footer });
  return e;
}

export function warnEmbed(title, description, footer) {
  const e = base(COLORS.WARNING).setTitle(`⚠️  ${title}`);
  if (description) e.setDescription(description);
  if (footer) e.setFooter({ text: footer });
  return e;
}

export function neutralEmbed(title, description, footer) {
  const e = base(COLORS.NEUTRAL).setTitle(title);
  if (description) e.setDescription(description);
  if (footer) e.setFooter({ text: footer });
  return e;
}
