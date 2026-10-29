// ── "Or create a new one" companions for role and channel pickers ──────────────
// Discord draws the role and channel picker of a slash command option itself, so a bot cannot add a
// "Create new…" entry to it. Instead a setup option such as `role` gets a string companion `new_role`:
// the admin either picks an existing role or types a name, and the command creates that role (or reuses
// one with exactly that name) and then runs as if it had been picked.
//
// Builders add the companion right after the picker with newRoleOption / newChannelOption (a picker that
// was required becomes optional). The command calls resolveNewOptions right after its own defer and
// before it reads the picker; from then on getRole/getChannel return the created role or channel.
// A private option (log or transcript channel) reuses an existing channel of that name only when its audience is no wider
// than a channel made here would have; otherwise it refuses, so a typed name never routes private logs into a channel
// someone else can read. reuseNote() tells the command to say so in its reply when one was reused.
// Nothing here throws: failures answer the interaction with an error and resolve to false.
import { ApplicationCommandOptionType, ChannelType, OverwriteType, PermissionFlagsBits, escapeMarkdown } from 'discord.js';
import logger from './logger.js';
import { errorEmbed } from './embedBuilder.js';
import { createKeyedMutex, withTimeout, TimeoutError } from './async.js';
import { safeReply, missingBotPermissions, permissionNames, DANGEROUS_PERMISSIONS } from './interactionHelpers.js';
import { memberChannelProblem } from './privacyGuards.js';
import { clampText } from './text.js';

export const NEW_PREFIX = 'new_';
const NAME_MAX = 100;
const CREATE_TIMEOUT_MS = 15_000;
const MEMBER_LOOKUP_MS = 3_000;

const { Role: ROLE, Channel: CHANNEL, String: STRING, Subcommand, SubcommandGroup } = ApplicationCommandOptionType;

// ── Builders ──────────────────────────────────────────────────────────────────

/** Companion `new_<base>` for a role picker named `base`. */
export function newRoleOption(o, base = 'role') {
  return o.setName(`${NEW_PREFIX}${base}`)
    .setDescription('Or type a name to create a new role (reuses an exact match).')
    .setMinLength(1).setMaxLength(NAME_MAX);
}

/** Companion `new_<base>` for a channel picker named `base`; `noun` is what the description calls it ("category", "voice channel"). */
export function newChannelOption(o, base = 'channel', noun = 'channel') {
  return o.setName(`${NEW_PREFIX}${base}`)
    .setDescription(`Or type a name to create a new ${noun} (reuses an exact match).`)
    .setMinLength(1).setMaxLength(NAME_MAX);
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

// First type a channel picker accepts that the bot can create; thread types are never created.
const CREATE_ORDER = [
  ChannelType.GuildText, ChannelType.GuildVoice, ChannelType.GuildCategory,
  ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildStageVoice,
];
const SLUGGED = new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum]);

/** The channel type to create for a picker's `channel_types` (none means any: text). null when only threads are allowed. */
export function channelTypeFor(channelTypes) {
  if (!channelTypes?.length) return ChannelType.GuildText;
  return CREATE_ORDER.find((t) => channelTypes.includes(t)) ?? null;
}

/** How Discord stores a text channel name: lowercase, spaces become dashes. */
const slug = (name) => name.trim().toLowerCase().replace(/\s+/g, '-');

/**
 * Existing item with that name: an exact (case-sensitive) match wins over a case-insensitive one.
 * Text-like channel names are compared in Discord's lowercase-dashed form.
 */
export function findByName(items, name, { slugged = false } = {}) {
  const want = slugged ? slug(name) : name.trim();
  const lower = want.toLowerCase();
  let loose = null;
  for (const item of items) {
    if (item.name === want) return item;
    if (!loose && item.name?.toLowerCase() === lower) loose = item;
  }
  return loose;
}

/** The option definitions of the subcommand the interaction ran (or the top level), from the command's builder. */
export function leafDefinitions(data, interaction) {
  let opts = (typeof data?.toJSON === 'function' ? data.toJSON() : data)?.options ?? [];
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand(false);
  if (group) opts = opts.find((o) => o.type === SubcommandGroup && o.name === group)?.options ?? [];
  if (sub) opts = opts.find((o) => o.type === Subcommand && o.name === sub)?.options ?? [];
  return opts;
}

/** Every `new_<base>` string option whose sibling `<base>` is a role or channel picker. */
export function companionPairs(defs) {
  const out = [];
  for (const d of defs) {
    if (d.type !== STRING || !d.name.startsWith(NEW_PREFIX)) continue;
    const base = defs.find((b) => b.name === d.name.slice(NEW_PREFIX.length));
    if (base && (base.type === ROLE || base.type === CHANNEL)) out.push({ newName: d.name, base });
  }
  return out;
}

/**
 * Makes `getRole(base)` / `getChannel(base)` (and `options.data` for subcommands) see the entity as if it had been picked,
 * replacing the `new_<base>` entry. `_hoistedOptions` is discord.js's private leaf-options array (pinned by a test).
 */
export function substituteOption(interaction, newName, base, kind, entity) {
  const hoisted = interaction.options._hoistedOptions;
  if (!Array.isArray(hoisted)) throw new Error('discord.js option resolver changed: _hoistedOptions is missing');
  const entry = kind === 'role'
    ? { name: base, type: ROLE, value: entity.id, role: entity }
    : { name: base, type: CHANNEL, value: entity.id, channel: entity };
  const i = hoisted.findIndex((o) => o.name === newName);
  if (i >= 0) hoisted.splice(i, 1, entry);
  else hoisted.push(entry);
}

function describeCreateError(err, noun) {
  if (err instanceof TimeoutError) {
    return `Discord took too long to create the ${noun}. Try again; if it appeared meanwhile, I'll reuse it.`;
  }
  switch (err?.code) {
    case 30005: return 'This server already has the maximum number of roles (250). Delete one or pick an existing role.';
    case 30013: return 'This server already has the maximum number of channels (500). Delete one or pick an existing channel.';
    case 50013: return `I'm missing a permission to create that ${noun}. Check my role's permissions and try again.`;
    case 50035: return `Discord rejected that name. Try a different ${noun} name.`;
    default: return `The ${noun} could not be created. Please try again in a moment.`;
  }
}

// ── Creation ──────────────────────────────────────────────────────────────────

// Serialises create-or-reuse per guild and name, so two admins typing the same name get one role or channel.
const createLock = createKeyedMutex();

const auditReason = (interaction) => clampText(
  `/${[interaction.commandName, interaction.options.getSubcommandGroup(false), interaction.options.getSubcommand(false)]
    .filter(Boolean).join(' ')} by ${interaction.user.username} (${interaction.user.id})`,
  500,
);

/** The invoker and the bot both need the permission to create (picking or reusing never needs it). */
function permissionProblem(interaction, perm, noun) {
  if (!interaction.memberPermissions?.has(PermissionFlagsBits[perm])) {
    return `You need the **${perm === 'ManageRoles' ? 'Manage Roles' : 'Manage Channels'}** permission to create a ${noun}. Pick an existing one instead.`;
  }
  if (missingBotPermissions(interaction.guild, [perm]).length) {
    return `I need the **${perm === 'ManageRoles' ? 'Manage Roles' : 'Manage Channels'}** permission to create a ${noun}.`;
  }
  return null;
}

async function obtainRole(interaction, name, baseName) {
  const { guild } = interaction;
  const reusable = () => findByName(
    guild.roles.cache.filter((r) => r.id !== guild.id && !r.managed).values(),
    name,
  );
  return createLock.run(`${guild.id}:role:${name.toLowerCase()}`, async () => {
    const existing = reusable();
    if (existing) {
      // Typing a name must never quietly hand out a powerful role that the admin did not deliberately pick.
      const dangerous = DANGEROUS_PERMISSIONS.filter((k) => existing.permissions?.has?.(PermissionFlagsBits[k], false));
      if (dangerous.length) {
        return {
          ok: false, title: 'Role Name Taken',
          message: `A role named **${escapeMarkdown(existing.name)}** already exists and has powerful permissions (${permissionNames(dangerous).join(', ')}). `
            + `Pick it in \`${baseName}\` if you really mean it, or type another name.`,
        };
      }
      return { ok: true, entity: existing, created: false };
    }
    const problem = permissionProblem(interaction, 'ManageRoles', 'role');
    if (problem) return { ok: false, message: problem };
    // A new role lands just above @everyone: with no role of its own the bot could not give it out, and the command
    // would refuse it after it was made, leaving it behind.
    const highest = guild.members.me?.roles?.highest;
    if (highest && highest.id === guild.id) {
      return {
        ok: false, title: 'Cannot Create Role',
        message: "I have no role of my own, so a role I create would sit above my highest role and I couldn't give it out. "
          + 'Give me a role near the top of **Server Settings → Roles** (or re-invite me so Discord adds one), then try again.',
      };
    }
    let role;
    try {
      role = await withTimeout(
        guild.roles.create({ name, permissions: [], mentionable: false, hoist: false, reason: auditReason(interaction) }),
        CREATE_TIMEOUT_MS, 'create role',
      );
    } catch (err) {
      logger.warn(`[newTargets] ${guild.id}: creating role "${name}" failed`, err);
      return { ok: false, message: describeCreateError(err, 'role') };
    }
    // The same test checkAssignableRole makes, so the role goes exactly when the command would refuse it.
    if (highest && role.position >= highest.position) {
      await role.delete(auditReason(interaction)).catch((err) => logger.warn(`[newTargets] ${guild.id}: could not delete role ${role.id}`, err));
      return {
        ok: false, title: 'Cannot Create Role',
        message: 'The new role ended up above my highest role, so I deleted it again. Move my role higher in **Server Settings → Roles**, then try again.',
      };
    }
    return { ok: true, entity: role, created: true };
  });
}

const PRIVATE_BOT_ALLOW = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles', 'ReadMessageHistory'];

/** Hidden from @everyone; visible to the bot (only permissions it has server-wide, or Discord refuses) and the invoker. */
function privateOverwrites(interaction) {
  const { guild } = interaction;
  const me = guild.members.me;
  const allow = PRIVATE_BOT_ALLOW.filter((k) => me?.permissions?.has(PermissionFlagsBits[k]));
  const out = [{ id: guild.id, deny: [PermissionFlagsBits.ViewChannel] }];
  if (me) out.push({ id: me.id, allow: allow.map((k) => PermissionFlagsBits[k]) });
  if (interaction.user.id !== me?.id) out.push({ id: interaction.user.id, allow: [PermissionFlagsBits.ViewChannel] });
  return out;
}

const WIDER_REASONS = Object.freeze({
  everyone: 'everyone can see it',
  invoker: 'you cannot see it',
  others: 'roles or members besides you, me and server admins can see it',
});

/** Whether a member holds Manage Server (or Administrator); false when that cannot be told (not cached and the fetch failed). */
async function isServerAdmin(guild, id) {
  let member = guild.members.cache?.get?.(id) ?? null;
  if (!member && typeof guild.members.fetch === 'function') {
    member = await withTimeout(Promise.resolve().then(() => guild.members.fetch(id)), MEMBER_LOOKUP_MS, 'fetch member').catch(() => null);
  }
  return member?.permissions?.has?.(PermissionFlagsBits.ManageGuild) === true;
}

/**
 * Why an existing channel may not stand in for a private one (see privateOverwrites): 'everyone' (@everyone can view it, or
 * that cannot be told), 'invoker' (the admin typing the name cannot view it, so could not tell where logs go), 'others' (a
 * role or member overwrite other than @everyone, the bot, the invoker and Manage Server / Administrator roles or members
 * allows ViewChannel), or null when it is no wider than a channel made here.
 */
async function privateReuseProblem(interaction, channel) {
  const { guild } = interaction;
  if (channel.permissionsFor?.(guild.id)?.has(PermissionFlagsBits.ViewChannel) ?? true) return 'everyone';
  const member = interaction.member ?? guild.members.cache?.get?.(interaction.user.id) ?? null;
  if (!member || memberChannelProblem(channel, member, ['ViewChannel'])) return 'invoker';
  const me = guild.members.me;
  const botRoleId = me?.roles?.botRole?.id;
  for (const ow of channel.permissionOverwrites?.cache?.values?.() ?? []) {
    if (!ow.allow?.has?.(PermissionFlagsBits.ViewChannel)) continue;
    if (ow.id === guild.id || ow.id === me?.id || ow.id === interaction.user.id || ow.id === botRoleId) continue;
    // has() counts Administrator too. A member that cannot be told (not cached, fetch failed) counts as wider.
    if (ow.type === OverwriteType.Role) {
      if (guild.roles.cache.get(ow.id)?.permissions?.has?.(PermissionFlagsBits.ManageGuild)) continue;
    } else if (await isServerAdmin(guild, ow.id)) continue; // e.g. another admin the bot made this channel for earlier
    return 'others';
  }
  return null;
}

/** interaction -> existing private channels used instead of creating one (see reuseNote). */
const reusedPrivate = new WeakMap();

/** The reply line for a command whose private option typed by name used an existing channel instead of creating one, else null. */
export function reuseNote(interaction) {
  const ids = reusedPrivate.get(interaction);
  return ids?.length ? `ℹ️ Reused the existing ${ids.map((id) => `<#${id}>`).join(', ')} instead of creating a new private channel.` : null;
}

async function obtainChannel(interaction, name, base, { hidden }) {
  const { guild } = interaction;
  const type = channelTypeFor(base.channel_types);
  const noun = type === ChannelType.GuildCategory ? 'category' : 'channel';
  if (type === null) return { ok: false, message: 'That option only accepts threads, which I cannot create. Pick an existing one.' };
  const allowed = base.channel_types?.length ? base.channel_types : [type];
  const slugged = SLUGGED.has(type);
  const key = slugged ? slug(name) : name.trim().toLowerCase();
  return createLock.run(`${guild.id}:channel:${type}:${key}`, async () => {
    const existing = findByName(guild.channels.cache.filter((c) => allowed.includes(c.type)).values(), name, { slugged });
    if (existing) {
      // A private (log) channel must not quietly become an existing one that more people can read than a new one would;
      // unknown visibility counts as public. Picking it in the picker stays possible (the admin sees it there).
      const wider = hidden ? await privateReuseProblem(interaction, existing) : null;
      if (wider) {
        const pickable = wider !== 'invoker'; // a channel the admin cannot view is not in the picker
        return {
          ok: false, title: 'Channel Name Taken',
          message: `${pickable ? `<#${existing.id}>` : `A ${noun} with that name`} already exists and ${WIDER_REASONS[wider]}, but this ${noun} is created private. `
            + (pickable ? `Pick it in \`${base.name}\` if you really mean it, or type another name.` : 'Type another name.'),
        };
      }
      return { ok: true, entity: existing, created: false };
    }
    const problem = permissionProblem(interaction, 'ManageChannels', noun);
    if (problem) return { ok: false, message: problem };
    try {
      const channel = await withTimeout(
        guild.channels.create({
          name, type, reason: auditReason(interaction),
          ...(hidden ? { permissionOverwrites: privateOverwrites(interaction) } : {}),
        }),
        CREATE_TIMEOUT_MS, 'create channel',
      );
      return { ok: true, entity: channel, created: true };
    } catch (err) {
      logger.warn(`[newTargets] ${guild.id}: creating ${noun} "${name}" failed`, err);
      return { ok: false, message: describeCreateError(err, noun) };
    }
  });
}

async function fail(interaction, title, message) {
  await safeReply(interaction, { embeds: [errorEmbed(title, message)], components: [], allowedMentions: { parse: [] } });
  return false;
}

/**
 * Creates (or reuses) every role or channel typed into a `new_<base>` companion and substitutes it for the picker.
 * Call it after the command deferred and before it reads any picker.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {import('discord.js').SlashCommandBuilder|object} data the command's builder (or its JSON)
 * @param {{ required?: string[], private?: string[] }} [opts]
 *   required: pickers that need a value from either the picker or its companion;
 *   private: channel pickers whose new channel is hidden from @everyone (logs, transcripts); an existing channel of the typed
 *     name is reused only when no more people can see it than a new one (otherwise "Channel Name Taken"), see reuseNote()
 * @returns {Promise<boolean>} true to continue; false when an error was already shown
 */
export async function resolveNewOptions(interaction, data, { required = [], private: hidden = [] } = {}) {
  try {
    for (const { newName, base } of companionPairs(leafDefinitions(data, interaction))) {
      const kind = base.type === ROLE ? 'role' : 'channel';
      const raw = interaction.options.getString(newName);
      const name = raw?.trim() ?? '';
      const picked = interaction.options.get(base.name) !== null;

      if (raw !== null && !name) return fail(interaction, 'Name Required', `Type a name in \`${newName}\`, or pick an existing ${kind} in \`${base.name}\`.`);
      if (name && picked) {
        return fail(interaction, 'Pick One', `Pick an existing ${kind} in \`${base.name}\` **or** type a new name in \`${newName}\`, not both.`);
      }
      if (!name) {
        if (!picked && required.includes(base.name)) {
          return fail(interaction, `Missing ${kind === 'role' ? 'Role' : 'Channel'}`, `Pick a ${kind} in \`${base.name}\`, or type a name in \`${newName}\` to create one.`);
        }
        continue;
      }

      const res = kind === 'role'
        ? await obtainRole(interaction, name, base.name)
        : await obtainChannel(interaction, name, base, { hidden: hidden.includes(base.name) });
      if (!res.ok) return fail(interaction, res.title ?? `Could Not Create ${kind === 'role' ? 'Role' : 'Channel'}`, res.message);
      if (res.created) logger.info(`[newTargets] ${interaction.guildId}: created ${kind} ${res.entity.id} "${name}" for ${auditReason(interaction)}`);
      else if (kind === 'channel' && hidden.includes(base.name)) reusedPrivate.set(interaction, [...(reusedPrivate.get(interaction) ?? []), res.entity.id]);
      substituteOption(interaction, newName, base.name, kind, res.entity);
    }
    return true;
  } catch (err) {
    logger.error('[newTargets] resolveNewOptions failed', err);
    return fail(interaction, 'Something Went Wrong', 'The role or channel could not be prepared. Please try again in a moment.');
  }
}
