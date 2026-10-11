import { PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../config/constants.js';

/**
 * Resolves a numeric permission level for a guild member.
 * OWNER = BOT_OWNER_ID; ADMIN = guild owner, Administrator or Manage Server;
 * MODERATOR = a role in GuildSettings.modRoleIds, or Kick/Ban/Moderate Members (R056-R059).
 * @param {import('discord.js').GuildMember} member
 * @param {object|null} guildSettings - Row from GuildSettings table (null when unavailable)
 * @returns {number} One of PERMISSION_LEVELS
 */
export function resolvePermissionLevel(member, guildSettings) {
  if (!member) return PERMISSION_LEVELS.MEMBER;
  if (member.id === process.env.BOT_OWNER_ID) return PERMISSION_LEVELS.OWNER;

  const perms = member.permissions; // PermissionsBitField (cached member)
  if (member.guild?.ownerId === member.id) return PERMISSION_LEVELS.ADMIN;
  if (perms?.has(PermissionFlagsBits.Administrator) || perms?.has(PermissionFlagsBits.ManageGuild)) {
    return PERMISSION_LEVELS.ADMIN;
  }

  const roleIds = member.roles?.cache ?? null;
  if ((guildSettings?.modRoleIds ?? []).some((id) => roleIds?.has(id))) return PERMISSION_LEVELS.MODERATOR;

  // R058: the Discord permissions every moderation command already advertises.
  if (
    perms?.has(PermissionFlagsBits.KickMembers) ||
    perms?.has(PermissionFlagsBits.BanMembers) ||
    perms?.has(PermissionFlagsBits.ModerateMembers)
  ) {
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
