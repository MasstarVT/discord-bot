// Shared access and privacy checks for features that copy or configure content across channels (starboards, automations,
// custom commands, saved embeds). Pure and synchronous; nothing here reads or writes anything.
import { ChannelType, PermissionFlagsBits } from 'discord.js';
import { permissionNames } from './interactionHelpers.js';

/**
 * Whether @everyone can view a channel. A private thread is never visible; any other thread follows its parent.
 * A channel object without permission data (never a real guild channel, only a test double) counts as visible.
 * @param {import('discord.js').GuildBasedChannel|null|undefined} channel
 * @returns {true|false|null} null = cannot be told (no channel, or the permissions could not be computed)
 */
export function everyoneCanView(channel) {
  if (!channel) return null;
  if (channel.type === ChannelType.PrivateThread) return false;
  if (typeof channel.permissionsFor !== 'function') return true;
  try {
    const everyone = channel.guild?.roles?.everyone ?? channel.guildId ?? channel.guild?.id;
    const perms = everyone ? channel.permissionsFor(everyone) : null;
    return perms ? perms.has(PermissionFlagsBits.ViewChannel) : null;
  } catch {
    return null;
  }
}

/**
 * True when copying content from `source` into `target` would show it to people who cannot see where it came from: the source
 * is hidden from @everyone (or cannot be told) and the target is visible to @everyone (or cannot be told). A private target may
 * take content from a private source, and a public source may go anywhere.
 */
export function copyLeaksPrivate(source, target) {
  if (source?.id && source.id === target?.id) return false;
  return everyoneCanView(source) !== true && everyoneCanView(target) !== false;
}

/**
 * The refusal text when the member who configures something cannot use a channel the way it will (view it, post in it), else
 * null. The guild owner and Administrators are exempt. A thread swaps SendMessages for SendMessagesInThreads. A missing member
 * or channel is not checked here.
 * @param {import('discord.js').GuildBasedChannel|null|undefined} channel
 * @param {import('discord.js').GuildMember|null|undefined} member
 * @param {string[]} [needs] PermissionFlagsBits keys
 * @returns {string|null}
 */
export function memberChannelProblem(channel, member, needs = ['ViewChannel', 'SendMessages']) {
  if (!channel || !member) return null;
  const guild = channel.guild ?? member.guild;
  if (member.id === guild?.ownerId || member.permissions?.has?.(PermissionFlagsBits.Administrator)) return null;
  const keys = channel.isThread?.() ? needs.map((k) => (k === 'SendMessages' ? 'SendMessagesInThreads' : k)) : needs;
  let perms = null;
  try {
    perms = channel.permissionsFor?.(member) ?? null;
  } catch {
    perms = null;
  }
  const missing = perms ? keys.filter((k) => !perms.has(PermissionFlagsBits[k])) : keys;
  if (!missing.length) return null;
  return `You need **${permissionNames(missing).join(', ')}** in <#${channel.id}> to do that.`;
}

/**
 * The refusal text when any of `roleIds` is at or above the member's highest role (the guild owner is exempt), else null. Roles
 * the server does not have are ignored; a member whose roles cannot be read fails closed.
 * @param {import('discord.js').Guild} guild
 * @param {import('discord.js').GuildMember|null|undefined} member
 * @param {Iterable<string>} roleIds
 * @returns {string|null}
 */
export function roleHierarchyProblem(guild, member, roleIds) {
  try {
    const ids = [...(roleIds ?? [])];
    if (!member || member.id === guild.ownerId || ids.length === 0) return null;
    const top = member.roles.highest.position;
    if (!Number.isFinite(top)) return 'Your roles could not be checked.';
    const above = ids.find((id) => {
      const role = guild.roles.cache.get(id);
      return role && role.position >= top;
    });
    return above ? `<@&${above}> is at or above your highest role.` : null;
  } catch {
    return 'Your roles could not be checked.';
  }
}
