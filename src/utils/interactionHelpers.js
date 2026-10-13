// ── Interaction helpers ────────────────────────────────────────────────────────
// Defer and reply without throwing, permission and role checks, autocomplete replies and a
// confirm dialog. Nothing here ever throws; failures are logged and reported through the
// return value.
import { randomBytes } from 'node:crypto';
import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  ComponentType,
  PermissionFlagsBits,
  PermissionsBitField,
} from 'discord.js';
import logger from './logger.js';
import { errorEmbed, infoEmbed, successEmbed, warnEmbed } from './embedBuilder.js';
import { clampText } from './text.js';

// ── Reply helpers ──────────────────────────────────────────────────────────────

const GONE_CODES = new Set([
  10062, // Unknown interaction
  40060, // Interaction has already been acknowledged
  10015, // Unknown webhook (the token expired)
  50027, // Invalid webhook token
]);

/**
 * True when an error means the interaction can no longer be answered (expired or already
 * acknowledged), which is normal and not worth a warning.
 * @param {unknown} err
 * @returns {boolean}
 */
export function isInteractionGone(err) {
  const code = err?.code;
  return GONE_CODES.has(code) || code === 'InteractionAlreadyReplied';
}

/**
 * Defers the reply unless the interaction is already acknowledged.
 * @param {import('discord.js').RepliableInteraction} interaction
 * @param {{ ephemeral?: boolean }} [opts]
 * @returns {Promise<boolean>} true when the interaction is acknowledged
 */
export async function safeDefer(interaction, { ephemeral = false } = {}) {
  try {
    if (interaction.deferred || interaction.replied) return true;
    await interaction.deferReply({ ephemeral });
    return true;
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`safeDefer: interaction gone (${err?.code})`);
    else logger.warn('safeDefer failed', err);
    return false;
  }
}

function withoutEphemeral(payload) {
  const { ephemeral: _ignored, ...rest } = payload;
  return rest;
}

/**
 * Replies in the way that fits the interaction's state:
 * - not acknowledged: reply
 * - deferred with deferReply: editReply (visibility was fixed when it was deferred, so
 *   `payload.ephemeral` has no effect), falling back to followUp when the original was deleted
 * - already replied, or acknowledged by deferUpdate/update: followUp (editReply would
 *   overwrite the panel the user clicked)
 * Errors that mean the interaction is gone are swallowed; the rest are logged.
 * @param {import('discord.js').RepliableInteraction} interaction
 * @param {string|object} payload
 * @returns {Promise<import('discord.js').Message|null>} the sent message, or null
 */
export async function safeReply(interaction, payload) {
  try {
    const p = typeof payload === 'string' ? { content: payload } : { ...payload };
    delete p.fetchReply;
    delete p.withResponse;

    if (!interaction.deferred && !interaction.replied) {
      const res = await interaction.reply({ ...p, withResponse: true });
      return res?.resource?.message ?? null;
    }
    // deferReply sets `ephemeral` to a boolean; deferUpdate and update leave it null.
    if (interaction.deferred && !interaction.replied && typeof interaction.ephemeral === 'boolean') {
      try {
        return await interaction.editReply(withoutEphemeral(p));
      } catch (err) {
        if (err?.code !== 10008) throw err; // 10008: the original was deleted (deleteReply)
        return await interaction.followUp(p);
      }
    }
    return await interaction.followUp(p);
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`safeReply: interaction gone (${err?.code})`);
    else logger.warn('safeReply failed', err);
    return null;
  }
}

/**
 * @param {import('discord.js').RepliableInteraction} interaction
 * @param {string} title
 * @param {string} [description]
 * @param {{ ephemeral?: boolean }} [opts]
 * @returns {Promise<import('discord.js').Message|null>}
 */
export async function replyError(interaction, title, description, { ephemeral = true } = {}) {
  return safeReply(interaction, { embeds: [errorEmbed(title, description)], ephemeral });
}

/**
 * @param {import('discord.js').RepliableInteraction} interaction
 * @param {string} title
 * @param {string} [description]
 * @param {{ ephemeral?: boolean }} [opts]
 * @returns {Promise<import('discord.js').Message|null>}
 */
export async function replySuccess(interaction, title, description, { ephemeral = false } = {}) {
  return safeReply(interaction, { embeds: [successEmbed(title, description)], ephemeral });
}

// ── Permissions ────────────────────────────────────────────────────────────────

const PERMISSION_LABELS = Object.freeze({
  ManageGuild: 'Manage Server',
  ModerateMembers: 'Timeout Members',
  CreateInstantInvite: 'Create Invite',
  ManageGuildExpressions: 'Manage Expressions',
  ManageEmojisAndStickers: 'Manage Emojis and Stickers',
  MentionEveryone: 'Mention @everyone',
  SendTTSMessages: 'Send TTS Messages',
  UseVAD: 'Use Voice Activity',
  ViewAuditLog: 'View Audit Log',
  SendMessagesInThreads: 'Send Messages in Threads',
  UseApplicationCommands: 'Use Application Commands',
});

function permissionKeys(x) {
  if (x == null) return [];
  if (Array.isArray(x)) return x.flatMap(permissionKeys);
  if (typeof x === 'string') return Object.hasOwn(PermissionFlagsBits, x) ? [x] : [];
  if (typeof x === 'bigint' || typeof x === 'number' || x instanceof PermissionsBitField) {
    try {
      return new PermissionsBitField(x).toArray();
    } catch {
      return [];
    }
  }
  return [];
}

/**
 * Display names for permissions: 'ManageRoles' becomes 'Manage Roles', 'ManageGuild' 'Manage Server'.
 * @param {bigint|string|Array<string|bigint>|import('discord.js').PermissionsBitField} bitfieldOrArray
 * @returns {string[]} unique, in input order
 */
export function permissionNames(bitfieldOrArray) {
  const names = permissionKeys(bitfieldOrArray).map(
    (k) => PERMISSION_LABELS[k] ?? k.replace(/([a-z])([A-Z])/g, '$1 $2'),
  );
  return [...new Set(names)];
}

const warnedKeys = new Set();

/**
 * The permissions the bot lacks in a channel (or guild-wide).
 * @param {import('discord.js').GuildBasedChannel|import('discord.js').Guild|null} channelOrGuild
 * @param {string|string[]} perms - PermissionFlagsBits keys, e.g. 'ManageRoles'
 * @returns {string[]} the missing keys (Administrator satisfies everything)
 */
export function missingBotPermissions(channelOrGuild, perms) {
  const keys = [];
  for (const k of Array.isArray(perms) ? perms : [perms]) {
    if (Object.hasOwn(PermissionFlagsBits, k)) keys.push(k);
    else if (!warnedKeys.has(k)) {
      warnedKeys.add(k);
      logger.warn(`missingBotPermissions: unknown permission "${k}" ignored`);
    }
  }
  let bits = null;
  try {
    if (typeof channelOrGuild?.permissionsFor === 'function') {
      const me = channelOrGuild.guild?.members?.me;
      bits = me ? channelOrGuild.permissionsFor(me) : null;
    } else {
      bits = channelOrGuild?.members?.me?.permissions ?? null;
    }
  } catch {
    bits = null;
  }
  if (bits == null) return keys;
  return keys.filter((k) => !bits.has(PermissionFlagsBits[k]));
}

/**
 * Replies with a 'Missing Permissions' error and returns false when the bot lacks permissions.
 * @param {import('discord.js').RepliableInteraction} interaction
 * @param {string|string[]} perms - PermissionFlagsBits keys
 * @param {{ channel?: import('discord.js').GuildBasedChannel, ephemeral?: boolean }} [opts]
 * @returns {Promise<boolean>} true when nothing is missing (or in a DM); false means already replied
 */
export async function requireBotPermissions(interaction, perms, { channel = interaction.channel, ephemeral = true } = {}) {
  try {
    if (!interaction.guild) return true;
    const target = channel ?? interaction.guild;
    const missing = missingBotPermissions(target, perms);
    if (missing.length === 0) return true;
    const names = permissionNames(missing).join(', ');
    const where = channel ? `in <#${channel.id}>` : 'in this server';
    await replyError(interaction, 'Missing Permissions', `I need **${names}** ${where} to do that.`, { ephemeral });
    return false;
  } catch (err) {
    logger.warn('requireBotPermissions failed', err);
    return false;
  }
}

/**
 * Whether the bot can post in a channel.
 * @param {import('discord.js').GuildBasedChannel|null} channel
 * @param {{ needs?: string[] }} [opts] - PermissionFlagsBits keys; threads swap SendMessages for SendMessagesInThreads
 * @returns {{ ok: boolean, missing: string[], reason?: string }}
 */
export function checkChannelUsable(channel, { needs = ['ViewChannel', 'SendMessages', 'EmbedLinks'] } = {}) {
  if (!channel) return { ok: false, missing: [], reason: 'Channel not found' };
  if (!channel.isTextBased?.()) return { ok: false, missing: [], reason: 'That is not a text channel' };
  const need = channel.isThread?.() ? needs.map((k) => (k === 'SendMessages' ? 'SendMessagesInThreads' : k)) : needs;
  const missing = missingBotPermissions(channel, need);
  if (missing.length === 0) return { ok: true, missing };
  return {
    ok: false,
    missing,
    reason: `I need **${permissionNames(missing).join(', ')}** in <#${channel.id}>.`,
  };
}

export const DANGEROUS_PERMISSIONS = [
  'Administrator', 'ManageGuild', 'ManageRoles', 'BanMembers', 'KickMembers',
  'ManageChannels', 'ManageWebhooks', 'MentionEveryone', 'ModerateMembers', 'ManageMessages',
];

/**
 * Whether the bot (and optionally the executor) may assign a role.
 * Checks, in order: exists, not @everyone, not managed, bot has Manage Roles, role below the
 * bot's highest role, role below the executor's highest role (the guild owner is exempt), and
 * then dangerous permissions.
 * @param {import('discord.js').Guild} guild
 * @param {import('discord.js').Role|null} role
 * @param {{ executor?: import('discord.js').GuildMember|null, allowDangerous?: boolean }} [opts]
 * @returns {{ ok: boolean, reason?: string, dangerous: string[] }} `dangerous` lists PermissionFlagsBits keys
 */
export function checkAssignableRole(guild, role, { executor = null, allowDangerous = false } = {}) {
  const fail = (reason) => ({ ok: false, reason, dangerous: [] });
  try {
    if (!role) return fail('Role not found.');
    if (role.id === guild.id) return fail('@everyone cannot be assigned.');
    if (role.managed) return fail('That role is managed by an integration.');
    const me = guild.members?.me;
    if (!me?.permissions?.has(PermissionFlagsBits.ManageRoles)) return fail('I need the Manage Roles permission.');
    if (role.position >= me.roles.highest.position) return fail('That role is above my highest role.');
    if (executor && executor.id !== guild.ownerId && role.position >= executor.roles.highest.position) {
      return fail('That role is above your highest role.');
    }
    // `false` = no administrator shortcut, so only the permissions the role really carries are listed.
    const dangerous = DANGEROUS_PERMISSIONS.filter((k) => role.permissions.has(PermissionFlagsBits[k], false));
    if (dangerous.length && !allowDangerous) {
      return {
        ok: false,
        reason: `That role has dangerous permissions: ${permissionNames(dangerous).join(', ')}.`,
        dangerous,
      };
    }
    return { ok: true, dangerous };
  } catch (err) {
    logger.warn('checkAssignableRole failed', err);
    return fail('That role could not be checked.');
  }
}

/**
 * The one role-ping rule. A role is allowed when it is mentionable, or when `executor` has
 * Mention Everyone in the channel; @everyone and unknown roles are refused.
 * - Configuration time: executor is the member running the command; refuse when `refused` is non-empty.
 * - Send time: executor is `guild.members.me`; pass only `allowed` to `allowedMentions.roles`.
 * Pure and synchronous.
 * @param {import('discord.js').GuildBasedChannel} channel
 * @param {import('discord.js').GuildMember|null} executor
 * @param {Iterable<string>} roleIds
 * @returns {{ allowed: string[], refused: string[] }}
 */
export function pingableRoleIds(channel, executor, roleIds) {
  const allowed = [];
  const refused = [];
  try {
    const guild = channel?.guild;
    let canMentionAll = null;
    for (const raw of new Set([...(roleIds ?? [])].map(String))) {
      const role = guild?.roles?.cache?.get(raw);
      if (!role || role.id === guild.id) { refused.push(raw); continue; }
      if (role.mentionable) { allowed.push(raw); continue; }
      canMentionAll ??= Boolean(executor && channel.permissionsFor(executor)?.has(PermissionFlagsBits.MentionEveryone));
      (canMentionAll ? allowed : refused).push(raw);
    }
  } catch (err) {
    logger.warn('pingableRoleIds failed', err);
  }
  return { allowed, refused };
}

// ── Autocomplete ───────────────────────────────────────────────────────────────

/**
 * Answers an autocomplete interaction: at most 25 choices, names and values cut to 100.
 * @param {import('discord.js').AutocompleteInteraction} interaction
 * @param {Array<{ name?: string, value: string|number }>} choices
 * @returns {Promise<void>}
 */
export async function respondAutocomplete(interaction, choices) {
  try {
    if (interaction.responded) return;
    const list = (Array.isArray(choices) ? choices : [])
      .slice(0, 25)
      .map((c) => ({
        name: clampText(String(c?.name ?? c?.value ?? ''), 100) || '—',
        value: typeof c?.value === 'string' ? c.value.slice(0, 100) : c?.value,
      }))
      .filter((c) => c.value !== '' && c.value !== undefined);
    await interaction.respond(list);
  } catch (err) {
    logger.debug(`respondAutocomplete failed: ${err?.message}`); // 10062 after 3 s is normal
  }
}

// ── Confirm dialog ─────────────────────────────────────────────────────────────

/**
 * Shows an embed with Confirm and Cancel buttons and waits for the invoker's answer.
 * Other users get an ephemeral "not for you" reply. After it resolves, the confirm message has
 * no buttons: report the result with `safeReply(interaction, ...)` (which follows up), or with
 * `interaction.editReply(...)` to overwrite the confirm message when it was the original reply.
 * Never throws; any error resolves false.
 * @param {import('discord.js').RepliableInteraction} interaction
 * @param {{ title?: string, description?: string, confirmLabel?: string, cancelLabel?: string,
 *   danger?: boolean, timeoutMs?: number, ephemeral?: boolean }} [opts]
 * @returns {Promise<boolean>} true only when the invoker confirmed
 */
export async function awaitConfirm(interaction, {
  title, description, confirmLabel = 'Confirm', cancelLabel = 'Cancel',
  danger = true, timeoutMs = 30_000, ephemeral = true,
} = {}) {
  try {
    const nonce = randomBytes(6).toString('hex');
    const yesId = `confirm:${nonce}:yes`;
    const noId = `confirm:${nonce}:no`;
    const make = danger ? warnEmbed : infoEmbed;
    const base = make(clampText(title || 'Are you sure?', 200), description ? clampText(description, 4000) : undefined);
    const row = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId(yesId).setLabel(clampText(confirmLabel, 80)).setStyle(danger ? ButtonStyle.Danger : ButtonStyle.Success),
      new ButtonBuilder().setCustomId(noId).setLabel(clampText(cancelLabel, 80)).setStyle(ButtonStyle.Secondary),
    );
    const payload = { embeds: [base], components: [row] };

    let message;
    if (!interaction.deferred && !interaction.replied) {
      const res = await interaction.reply({ ...payload, ephemeral, withResponse: true });
      message = res?.resource?.message ?? await interaction.fetchReply();
    } else if (interaction.deferred && !interaction.replied && typeof interaction.ephemeral === 'boolean') {
      message = await interaction.editReply(payload);
    } else {
      message = await interaction.followUp({ ...payload, ephemeral });
    }

    const time = Math.min(840_000, Math.max(5_000, Math.floor(Number(timeoutMs)) || 30_000));
    const collector = message.createMessageComponentCollector({
      componentType: ComponentType.Button,
      time,
      max: 1,
      filter: (i) => i.user.id === interaction.user.id && i.customId.startsWith(`confirm:${nonce}:`),
    });

    // Strangers: answer them instead of letting the router say "This menu has expired.".
    collector.on('ignore', (i) => {
      try {
        if (!i.customId?.startsWith('confirm:')) return;
        i.reply({ content: "These buttons aren't for you.", ephemeral: true }).catch(() => {});
      } catch {
        // ignore
      }
    });

    return await new Promise((resolve) => {
      let picked = null;
      collector.on('collect', (i) => { picked = i; });
      collector.on('end', async () => {
        let result = false;
        try {
          if (picked) {
            result = picked.customId === yesId;
            await picked.update({ embeds: [base.setFooter({ text: result ? 'Confirmed' : 'Cancelled' })], components: [] });
          } else {
            const edit = { embeds: [base.setFooter({ text: 'Timed out — nothing was changed.' })], components: [] };
            try {
              await interaction.editReply({ message: message.id, ...edit });
            } catch {
              await message.edit(edit).catch(() => {});
            }
          }
        } catch (err) {
          logger.debug(`awaitConfirm end failed: ${err?.message}`);
        }
        resolve(result);
      });
    });
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`awaitConfirm: interaction gone (${err?.code})`);
    else logger.warn('awaitConfirm failed', err);
    return false;
  }
}
