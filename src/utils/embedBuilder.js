import { EmbedBuilder } from 'discord.js';
import { COLORS } from '../config/constants.js';
import { clampText } from './text.js';

// Discord limits. Every factory clamps its inputs before calling discord.js's validating
// setters, so an over-long value can no longer throw.
const TITLE_MAX = 256;
const DESCRIPTION_MAX = 4096;
const FOOTER_MAX = 2048;

function base(color) {
  return new EmbedBuilder().setColor(color).setTimestamp();
}

function fill(e, description, footer) {
  if (description) e.setDescription(clampText(description, DESCRIPTION_MAX));
  if (footer) e.setFooter({ text: clampText(footer, FOOTER_MAX) });
  return e;
}

export function successEmbed(title, description, footer) {
  return fill(base(COLORS.SUCCESS).setTitle(clampText(`✅  ${title}`, TITLE_MAX)), description, footer);
}

export function errorEmbed(title, description, footer) {
  return fill(base(COLORS.ERROR).setTitle(clampText(`❌  ${title}`, TITLE_MAX)), description, footer);
}

export function infoEmbed(title, description, footer) {
  return fill(base(COLORS.INFO).setTitle(clampText(`ℹ️  ${title}`, TITLE_MAX)), description, footer);
}

export function warnEmbed(title, description, footer) {
  return fill(base(COLORS.WARNING).setTitle(clampText(`⚠️  ${title}`, TITLE_MAX)), description, footer);
}

export function neutralEmbed(title, description, footer) {
  const e = base(COLORS.NEUTRAL);
  if (title) e.setTitle(clampText(title, TITLE_MAX));
  return fill(e, description, footer);
}

/**
 * Applies the guild's configured embed colour. Use it on info and neutral embeds only; the
 * success, error and warning colours stay fixed.
 * @param {EmbedBuilder} embed
 * @param {{ embedColor?: number|null }|null} settings - a loaded GuildSettings row (may be null)
 * @returns {EmbedBuilder} the same embed
 */
export function applyGuildColor(embed, settings) {
  const c = settings?.embedColor;
  if (Number.isInteger(c) && c >= 0 && c <= 0xffffff) embed.setColor(c);
  return embed;
}

/**
 * Like neutralEmbed (no emoji prefix), but coloured with the guild's embed colour
 * (COLORS.INFO when none is set).
 * @param {{ embedColor?: number|null }|null} settings
 * @param {string} [title]
 * @param {string} [description]
 * @param {string} [footer]
 * @returns {EmbedBuilder}
 */
export function guildEmbed(settings, title, description, footer) {
  const e = applyGuildColor(base(COLORS.INFO), settings);
  if (title) e.setTitle(clampText(title, TITLE_MAX));
  return fill(e, description, footer);
}
