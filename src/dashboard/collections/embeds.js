// The `embeds` collection (Embed Messages): saved embeds (SavedEmbed rows holding a MessageTemplate) that the dashboard
// can create, edit, post in a channel, move, update in place and unpost, like /embed does. Also home of the pure helpers
// the collections-core stores share: rendering a template into a Discord REST message without a gateway client, the
// role-ping and role-assignment policies the slash commands apply to the member running them, and the mapping of
// Discord REST failures to RecordErrors. Pure: imports only pure helpers, nothing is read or connected at import time.
import { PermissionFlagsBits } from 'discord.js';
import { LIMITS } from '../../config/constants.js';
import { normalizeTemplate } from '../../utils/customEmbed.js';
import { RecordError } from './errors.js';

// ── Shared constants ─────────────────────────────────────────────────────────

/** Channel kinds a bot message can be posted in (text and announcement channels, as the slash commands allow). */
export const POSTABLE_CHANNEL_KINDS = Object.freeze(['text', 'announcement']);

/**
 * A name slug: what normalizeEmbedName (embedService) and sanitizeName (utils/text) leave unchanged, at most 32
 * characters: a-z, 0-9, - and _, no leading, trailing or doubled hyphen. RegExp source for the `pattern` key.
 */
export const SLUG_PATTERN = '(?!-)(?!.*--)[a-z0-9_-]{1,32}(?<!-)';
export const SLUG_MESSAGE = 'Use lowercase letters, numbers, - and _ only (up to 32 characters), e.g. server-rules.';

const CDN = 'https://cdn.discordapp.com';
const IMAGE_HASH_RE = /^(?:a_)?[0-9a-f]{32}$/;
const SNOWFLAKE_RE = /^\d{17,20}$/;

// ── The collection ───────────────────────────────────────────────────────────

export const collection = Object.freeze({
  key: 'embeds',
  module: 'embeds',
  label: 'Saved embeds',
  singular: 'saved embed',
  description: 'Build messages with an embed and link buttons, post them in any channel and edit the posted message '
    + 'whenever you like. Saved embeds are shared with /embed and the Automations module.',
  limit: LIMITS.SAVED_EMBEDS,
  titleField: 'name',
  subtitleField: 'status',
  fields: Object.freeze([
    {
      field: 'name',
      type: 'text',
      label: 'Name',
      description: 'A short unique name for this embed, also used by /embed in Discord. It cannot be changed later.',
      maxLength: 32,
      minLength: 1,
      pattern: SLUG_PATTERN,
      patternMessage: SLUG_MESSAGE,
      required: true,
    },
    {
      field: 'template',
      type: 'template',
      label: 'Message',
      description: 'The text, embed and link buttons. Variables like {server} are filled in when the message is posted; '
        + '{user} is you. Saving changes also updates the posted message.',
      requireContent: true,
      required: true,
      placeholders: [],
      default: { content: null, embed: { title: 'New embed', description: 'Write your message here.' }, buttons: [] },
    },
  ]),
  createOnly: Object.freeze(['name']),
  readOnly: Object.freeze([
    { field: 'status', type: 'text', label: 'Status', description: 'Draft, or posted in a channel.', maxLength: 40, default: 'Draft' },
    {
      field: 'channelId',
      type: 'channel',
      label: 'Channel',
      description: 'Where the message is posted (for a draft: where it was last posted).',
      channelTypes: [...POSTABLE_CHANNEL_KINDS],
      nullable: true,
      default: null,
    },
    { field: 'messageId', type: 'text', label: 'Message ID', maxLength: 20, nullable: true, default: null },
    { field: 'messageUrl', type: 'url', label: 'Posted message', description: 'Open the message in Discord.', nullable: true, default: null },
    { field: 'updatedAt', type: 'datetime', label: 'Last edited', nullable: true, default: null },
  ]),
  actions: Object.freeze([
    {
      id: 'publish',
      label: 'Post in a channel',
      description: 'Posts the message in a channel. If it is already posted, the new copy becomes the one that is kept up to date.',
      fields: [
        {
          field: 'channelId',
          type: 'channel',
          label: 'Channel',
          description: 'A text or announcement channel the bot can post in.',
          channelTypes: [...POSTABLE_CHANNEL_KINDS],
          required: true,
        },
        {
          field: 'deletePrevious',
          type: 'boolean',
          label: 'Delete the previous copy',
          description: 'When the embed is already posted somewhere, delete that message (move it). '
            + 'Off: the old message stays but is no longer updated.',
          default: true,
        },
      ],
    },
    {
      id: 'refresh',
      label: 'Update posted message',
      description: 'Applies the saved message to the posted copy again, e.g. after the server name changed or a missing permission was fixed.',
      fields: [],
    },
    {
      id: 'unpublish',
      label: 'Delete posted message',
      description: 'Deletes the posted message from Discord. The saved embed stays as a draft you can post again.',
      danger: true,
      confirm: 'Delete the posted message from Discord? The saved embed stays as a draft.',
      fields: [],
    },
  ]),
  // Deleting a saved embed keeps its posted message (like /embed delete without delete_message): unpost it first.
});

// ── Items ────────────────────────────────────────────────────────────────────

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);

/**
 * The jump link of a posted message.
 * @returns {string|null}
 */
export function messageUrl(guildId, channelId, messageId) {
  return SNOWFLAKE_RE.test(String(guildId)) && SNOWFLAKE_RE.test(String(channelId)) && SNOWFLAKE_RE.test(String(messageId))
    ? `https://discord.com/channels/${guildId}/${channelId}/${messageId}`
    : null;
}

/**
 * A SavedEmbed row as a collection item (wire shapes).
 * @param {object} row
 * @returns {object}
 */
export function embedItem(row) {
  const posted = Boolean(row.messageId && row.channelId);
  return {
    id: row.id,
    name: row.name,
    template: normalizeTemplate(row.template),
    status: posted ? 'Posted' : 'Draft',
    channelId: row.channelId ?? null,
    messageId: row.messageId ?? null,
    messageUrl: posted ? messageUrl(row.guildId, row.channelId, row.messageId) : null,
    updatedAt: iso(row.updatedAt),
  };
}

// ── Rendering without a gateway client ───────────────────────────────────────

/** The icon URL of a guild ({ id, icon }), animated icons as GIF like discord.js's iconURL(); null without an icon. */
export function guildIconUrl(guild) {
  if (!SNOWFLAKE_RE.test(String(guild?.id)) || typeof guild?.icon !== 'string' || !IMAGE_HASH_RE.test(guild.icon)) return null;
  return `${CDN}/icons/${guild.id}/${guild.icon}.${guild.icon.startsWith('a_') ? 'gif' : 'png'}?size=256`;
}

/** The avatar URL of a user ({ id, avatar? }), the default avatar when there is none. */
export function userAvatarUrl(user) {
  const id = SNOWFLAKE_RE.test(String(user?.id)) ? String(user.id) : '0';
  if (typeof user?.avatar === 'string' && IMAGE_HASH_RE.test(user.avatar)) {
    return `${CDN}/avatars/${id}/${user.avatar}.${user.avatar.startsWith('a_') ? 'gif' : 'png'}?size=256`;
  }
  return `${CDN}/embed/avatars/${Number((BigInt(id) >> 22n) % 6n)}.png`;
}

/** The creation time (ms) encoded in a snowflake, or undefined. */
function snowflakeTime(id) {
  try {
    return SNOWFLAKE_RE.test(String(id)) ? Number((BigInt(id) >> 22n) + 1_420_070_400_000n) : undefined;
  } catch {
    return undefined;
  }
}

/**
 * The placeholder render context (src/utils/placeholderParser.js) for a message the dashboard posts: the guild and
 * channel come from getGuildResources, {user} is the dashboard user (there is no member object).
 * @param {{ guild?: object, channels?: object[] }} resources getGuildResources(guildId)
 * @param {{ channelId?: string|null, user?: { id: string, username?: string, globalName?: string|null, avatar?: string|null }|null,
 *           userId?: string|null }} opts
 * @returns {{ member: null, user: object|null, guild: object, channel: object|null }}
 */
export function renderContextFor(resources, { channelId = null, user = null, userId = null } = {}) {
  const g = resources?.guild ?? {};
  const guild = {
    id: g.id,
    name: typeof g.name === 'string' ? g.name : '',
    memberCount: Number.isFinite(g.memberCount) ? g.memberCount : 0,
    ownerId: typeof g.ownerId === 'string' ? g.ownerId : null,
    createdTimestamp: snowflakeTime(g.id),
    premiumTier: Number.isInteger(g.premiumTier) ? g.premiumTier : 0,
    premiumSubscriptionCount: Number.isInteger(g.premiumSubscriptionCount) ? g.premiumSubscriptionCount : 0,
    verificationLevel: Number.isInteger(g.verificationLevel) ? g.verificationLevel : 0,
    iconURL: () => guildIconUrl(g),
  };
  let channel = null;
  if (channelId) {
    const c = (resources?.channels ?? []).find((x) => x.id === channelId);
    channel = { id: channelId, name: c?.name ?? '', type: Number.isInteger(c?.type) ? c.type : 0 };
  }
  const id = user?.id ?? userId;
  const u = SNOWFLAKE_RE.test(String(id))
    ? {
      id: String(id),
      username: typeof user?.username === 'string' ? user.username : '',
      globalName: typeof user?.globalName === 'string' ? user.globalName : null,
      discriminator: '0',
      bot: false,
      createdTimestamp: snowflakeTime(id),
      displayAvatarURL: () => userAvatarUrl({ id, avatar: user?.avatar }),
    }
    : null;
  return { member: null, user: u, guild, channel };
}

/** `allowed_mentions` for a REST message: users only (plus the given roles); @everyone and @here never ping. */
export function restAllowedMentions({ users = true, roles = [] } = {}) {
  const ids = [...new Set((roles ?? []).filter((id) => SNOWFLAKE_RE.test(String(id))))].slice(0, 100);
  return { parse: users ? ['users'] : [], ...(ids.length ? { roles: ids } : {}) };
}

/**
 * Converts the payload of renderTemplate (src/utils/customEmbed.js: builders) into a Discord REST message body.
 * @param {{ content?: string, embeds?: object[], components?: object[] }} rendered
 * @param {{ edit?: boolean, allowedMentions?: object }} [opts] edit: an empty content clears the old text
 * @returns {{ content?: string, embeds: object[], components: object[], allowed_mentions: object }}
 */
export function toRestMessage(rendered, { edit = false, allowedMentions = restAllowedMentions() } = {}) {
  const json = (x) => (x && typeof x.toJSON === 'function' ? x.toJSON() : x);
  const body = {
    embeds: (rendered?.embeds ?? []).map(json),
    components: (rendered?.components ?? []).map(json),
    allowed_mentions: allowedMentions,
  };
  if (typeof rendered?.content === 'string' && rendered.content !== '') body.content = rendered.content;
  else if (edit) body.content = '';
  return body;
}

// ── Who may do what (the slash commands check the member running them) ─────

const has = (bits, flag) => {
  try {
    return (BigInt(bits) & flag) === flag;
  } catch {
    return false;
  }
};

/**
 * The dashboard user as the role rules see them: `ctx.member` (the OAuth guild entry `{ owner, permissions }`, when the
 * server passes it) with `owner` also set when getGuildResources names them the server owner.
 * @returns {{ owner?: boolean, permissions?: string, highestRolePosition?: number }|null}
 */
export function memberFor(ctx, resources) {
  const m = ctx?.member ?? null;
  const ownerId = resources?.guild?.ownerId;
  if (m?.owner !== true && ownerId && ctx?.userId && ownerId === ctx.userId) return { ...(m ?? {}), owner: true };
  return m;
}

/**
 * Whether the dashboard user may ping every role: the server owner, Administrator or Mention @everyone (guild-level
 * permissions from the OAuth guild list). `member` = ctx.member `{ owner, permissions }`; unknown = false.
 */
export function memberCanMentionAll(member) {
  if (!member) return false;
  if (member.owner === true) return true;
  return has(member.permissions, PermissionFlagsBits.Administrator) || has(member.permissions, PermissionFlagsBits.MentionEveryone);
}

const roleOf = (resources, id) => (resources?.roles ?? []).find((r) => r.id === id) ?? null;
const roleName = (role, id) => (role?.name ? `@${role.name}` : `the role ${id}`);

/**
 * The ping-role rule of /reminder, /feed and /tag (pingableRoleIds with the member as executor): a role may be pinged
 * when it is mentionable or the member may mention every role. @everyone never.
 * @param {object} resources getGuildResources
 * @param {string} roleId
 * @param {object|null} member ctx.member
 * @returns {string|null} the problem, or null
 */
export function pingRoleProblem(resources, roleId, member) {
  const role = roleOf(resources, roleId);
  if (!role) return 'That role does not exist in this server any more. Pick another one.';
  if (role.everyone || role.id === resources?.guild?.id) return "@everyone can't be pinged. Use a role members can opt into.";
  if (role.mentionable === true || memberCanMentionAll(member)) return null;
  if (role.mentionable === false && member) {
    return `${roleName(role, roleId)} is not mentionable and you lack the Mention @everyone, @here and All Roles permission. `
      + 'Make the role mentionable in Server Settings → Roles, or pick another one.';
  }
  return `The dashboard could not check whether you may ping ${roleName(role, roleId)}. Make the role mentionable in `
    + 'Server Settings → Roles, or set it with the slash command in Discord.';
}

/**
 * The role-ping rule of /embed publish (resolvePublishMentions with the member running it): a `<@&id>` typed in the
 * text pings only when the role is mentionable or the member may mention every role. @everyone and unknown roles never.
 * @param {string[]} roleIds the role mentions found in the rendered text
 * @returns {{ allowed: string[], refused: string[] }}
 */
export function splitPingableRoles(roleIds, resources, member) {
  const allowed = [];
  const refused = [];
  for (const id of new Set(roleIds ?? [])) {
    const role = roleOf(resources, id);
    const ok = role && !role.everyone && id !== resources?.guild?.id && (role.mentionable === true || memberCanMentionAll(member));
    (ok ? allowed : refused).push(id);
  }
  return { allowed, refused };
}

/** Permissions a role handed out by a custom command must not carry (checkAssignableRole's DANGEROUS_PERMISSIONS). */
export const DANGEROUS_ROLE_PERMISSIONS = Object.freeze([
  'Administrator', 'ManageGuild', 'ManageRoles', 'BanMembers', 'KickMembers',
  'ManageChannels', 'ManageWebhooks', 'MentionEveryone', 'ModerateMembers', 'ManageMessages',
]);

const PERMISSION_LABELS = Object.freeze({
  Administrator: 'Administrator', ManageGuild: 'Manage Server', ManageRoles: 'Manage Roles', BanMembers: 'Ban Members',
  KickMembers: 'Kick Members', ManageChannels: 'Manage Channels', ManageWebhooks: 'Manage Webhooks',
  MentionEveryone: 'Mention @everyone', ModerateMembers: 'Timeout Members', ManageMessages: 'Manage Messages',
});

/**
 * The role-action rule of /tag action add (checkAssignableRole, allowDangerous false): not @everyone, not managed,
 * below the bot's highest role and without moderator/admin permissions (getGuildResources' `dangerous` flag, or the
 * role's `permissions` bits when present; unknown = refused). The member-hierarchy part (below the user's highest
 * role, the owner exempt) is checked here when `member.highestRolePosition` is known; the dashboard server checks it
 * for every new assignable role id before a write reaches a store.
 * @returns {string|null} the problem, or null
 */
export function assignRoleProblem(resources, roleId, member) {
  const role = roleOf(resources, roleId);
  if (!role) return 'That role does not exist in this server any more. Pick another one.';
  if (role.everyone || role.id === resources?.guild?.id) return '@everyone cannot be given or removed.';
  if (role.managed) return 'That role is managed by an integration and cannot be given by the bot.';
  if (role.assignable === false) {
    return "That role is above the bot's highest role. Move the bot's role higher in Server Settings → Roles, or pick a lower role.";
  }
  if (member?.owner !== true && Number.isInteger(member?.highestRolePosition) && role.position >= member.highestRolePosition) {
    return 'That role is not below your highest role, so you cannot have the bot give it.';
  }
  if (typeof role.permissions === 'string' || typeof role.permissions === 'bigint') {
    const bad = DANGEROUS_ROLE_PERMISSIONS.filter((k) => has(role.permissions, PermissionFlagsBits[k]));
    if (bad.length) return `That role has dangerous permissions: ${bad.map((k) => PERMISSION_LABELS[k]).join(', ')}.`;
    return null;
  }
  if (role.dangerous === true) {
    return 'That role has moderator or admin permissions (such as Administrator, Manage Server, Ban or Kick Members). '
      + 'Custom commands never hand those out; pick a role without them.';
  }
  if (role.dangerous === false) return null;
  return "The dashboard could not check this role's permissions. Add role actions with /tag action add in Discord.";
}

// ── Discord REST failures ────────────────────────────────────────────────────

/** True for a DiscordApiError of src/dashboard/discordApi.js (duck-typed: no import of the client). */
export const isDiscordApiError = (err) => err?.name === 'DiscordApiError' && typeof err.code === 'string';

/** Discord JSON error codes the stores branch on. */
export const DISCORD_CODES = Object.freeze({
  UNKNOWN_CHANNEL: 10003, UNKNOWN_MESSAGE: 10008, MISSING_ACCESS: 50001, CANNOT_EDIT_OTHERS: 50005,
  MISSING_PERMISSIONS: 50013, INVALID_FORM_BODY: 50035,
});

const channelLabel = (channel, id) => (channel?.name ? `#${channel.name}` : `the channel ${id ?? ''}`.trim());

/**
 * Maps a failed Discord write to a RecordError the admin can act on. Non-Discord errors are returned unchanged
 * (the caller rethrows them: a programming error is a 500, not a "Discord failed").
 * @param {unknown} err
 * @param {{ op: 'send'|'edit'|'delete', channel?: { id?: string, name?: string }|null, channelId?: string, embeds?: boolean }} info
 * @returns {Error}
 */
export function discordFailure(err, { op, channel = null, channelId = null, embeds = true }) {
  if (!isDiscordApiError(err)) return err;
  const where = channelLabel(channel, channelId ?? channel?.id);
  if (err.code === 'forbidden' || err.discordCode === DISCORD_CODES.MISSING_ACCESS || err.discordCode === DISCORD_CODES.MISSING_PERMISSIONS) {
    if (op === 'send') {
      const needs = `View Channel, Send Messages${embeds ? ' and Embed Links' : ''}`;
      return new RecordError('discord', `The bot can't post in ${where}: it needs ${needs} there.`);
    }
    if (op === 'edit') {
      return new RecordError('discord',
        `The bot can't edit its message in ${where}: it needs View Channel, Read Message History and Embed Links there.`);
    }
    return new RecordError('discord', `The bot can't delete the message in ${where}: it needs View Channel there.`);
  }
  if (err.discordCode === DISCORD_CODES.INVALID_FORM_BODY || err.status === 400) {
    return new RecordError('invalid', 'Discord rejected the message. Check the links and images (Discord must be able to load them).', {
      path: 'template',
    });
  }
  if (err.code === 'not_found') return new RecordError('discord', `${where[0].toUpperCase()}${where.slice(1)} no longer exists.`);
  if (err.code === 'rate_limited') return new RecordError('discord', 'Discord is rate limiting the bot right now. Try again in a few seconds.');
  return new RecordError('discord', 'Discord did not answer. Try again in a moment.');
}
