import { PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../config/constants.js';

/**
 * Resolves a numeric permission level for a guild member.
 * @param {import('discord.js').GuildMember} member
 * @param {object} guildSettings - Row from GuildSettings table
 * @returns {number} One of PERMISSION_LEVELS
 */
export function resolvePermissionLevel(member, guildSettings) {
  if (member.id === process.env.BOT_OWNER_ID) return PERMISSION_LEVELS.OWNER;

  if (member.permissions.has(PermissionFlagsBits.Administrator)) {
    return PERMISSION_LEVELS.ADMIN;
  }

  const modRoles = guildSettings?.modRoleIds ?? [];
  if (modRoles.some((roleId) => member.roles.cache.has(roleId))) {
    return PERMISSION_LEVELS.MODERATOR;
  }

  return PERMISSION_LEVELS.MEMBER;
}

/**
 * Returns true if member meets or exceeds the required permission level.
 * @param {import('discord.js').GuildMember} member
 * @param {number} required - PERMISSION_LEVELS constant
 * @param {object} guildSettings
 */
export function hasPermissionLevel(member, required, guildSettings) {
  return resolvePermissionLevel(member, guildSettings) >= required;
}
