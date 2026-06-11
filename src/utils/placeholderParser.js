// ── Ordinal helper ─────────────────────────────────────────────────────────────

function ordinal(n) {
  const formatted = n.toLocaleString();
  const v = n % 100;
  const suffix = v >= 11 && v <= 13 ? 'th'
    : ['th', 'st', 'nd', 'rd'][(n % 10)] ?? 'th';
  return `${formatted}${suffix}`;
}

// ── Placeholder registry ───────────────────────────────────────────────────────

/**
 * Replaces all supported placeholder tokens in a string.
 *
 * Supported tokens:
 *   {user.mention}              → <@userId>
 *   {user.username}             → username
 *   {user.tag}                  → username#1234
 *   {user.id}                   → snowflake string
 *   {user.avatar}               → avatar URL
 *   {user.created}              → Discord timestamp <t:...:R>
 *   {server.name}               → guild name
 *   {server.id}                 → guild ID
 *   {server.member_count}       → formatted number (e.g. "1,234")
 *   {server.member_count.ordinal} → ordinal (e.g. "1,234th")
 *   {server.icon}               → guild icon URL or empty string
 *
 * @param {string} template
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Guild} guild
 * @returns {string}
 */
export function parsePlaceholders(template, member, guild) {
  if (!template) return '';

  const user = member.user;
  const createdTs = Math.floor(user.createdTimestamp / 1000);

  const map = {
    '{user.mention}':               `<@${user.id}>`,
    '{user.username}':              user.username,
    '{user.tag}':                   user.tag,
    '{user.id}':                    user.id,
    '{user.avatar}':                user.displayAvatarURL({ size: 256 }),
    '{user.created}':               `<t:${createdTs}:R>`,
    '{server.name}':                guild.name,
    '{server.id}':                  guild.id,
    '{server.member_count}':        guild.memberCount.toLocaleString(),
    '{server.member_count.ordinal}': ordinal(guild.memberCount),
    '{server.icon}':                guild.iconURL({ size: 256 }) ?? '',
  };

  return template.replace(/\{[\w.]+\}/g, (token) => map[token] ?? token);
}

/** Returns a formatted list of all placeholder tokens for help text. */
export const PLACEHOLDER_LIST = [
  '`{user.mention}` — @mention the user',
  '`{user.username}` — their username',
  '`{user.tag}` — username#discriminator',
  '`{user.id}` — their user ID',
  '`{user.created}` — account creation date (relative)',
  '`{server.name}` — server name',
  '`{server.member_count}` — total member count',
  '`{server.member_count.ordinal}` — e.g. "1,234th"',
].join('\n');
