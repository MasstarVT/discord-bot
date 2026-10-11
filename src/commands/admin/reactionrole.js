// /reactionrole create | attach | add | remove | edit | repost | delete | list (pkg-reaction-roles.md section 4).
// Admin config command: replies are public (CLAUDE.md admin convention) except the delete prompt, and it keeps working while the
// Reaction Roles module is off (gated = false). Every query goes through the service; this file never touches Prisma or Redis.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { randomUUID } from 'node:crypto';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed, applyGuildColor } from '../../utils/embedBuilder.js';
import {
  checkAssignableRole, checkChannelUsable, permissionNames, respondAutocomplete, awaitConfirm, safeDefer, isInteractionGone,
} from '../../utils/interactionHelpers.js';
import { paginate } from '../../utils/paginator.js';
import { clampText } from '../../utils/text.js';
import { markBotDeletion } from '../../utils/deletionTracker.js';
import { getSettingsSafe, ensureGuildSettings, isModuleEnabled } from '../../services/settingsService.js';
import * as rr from '../../services/reactionRoleService.js';
import logger from '../../utils/logger.js';

export const module = 'reactionroles';
export const gated = false; // config works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const TYPE_CHOICES = [
  { name: 'Normal (get and remove)', value: 'NORMAL' },
  { name: 'Unique (one at a time)', value: 'UNIQUE' },
  { name: 'Verify (get only)', value: 'VERIFY' },
  { name: 'Reverse (removes it)', value: 'REVERSE' },
];
const LAYOUT_CHOICES = [
  { name: 'Buttons', value: 'BUTTONS' },
  { name: 'Dropdown', value: 'SELECT' },
  { name: 'Emoji reactions', value: 'REACTIONS' },
];
const STYLE_CHOICES = [
  { name: 'Blue', value: 'Primary' },
  { name: 'Grey', value: 'Secondary' },
  { name: 'Green', value: 'Success' },
  { name: 'Red', value: 'Danger' },
];
const TEMPLATE_CHOICES = [
  { name: 'Verify', value: 'verify' },
  { name: 'Notifications', value: 'notifications' },
];
const PANEL_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const panelOption = (o) => o.setName('panel').setDescription('Panel (type to search)').setRequired(true).setAutocomplete(true).setMaxLength(100);

export const data = new SlashCommandBuilder()
  .setName('reactionrole')
  .setDescription('Role panels with buttons, a dropdown or emoji reactions (MEE6 Reaction Roles)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('create').setDescription('Post a new role panel')
    .addChannelOption((o) => o.setName('channel').setDescription('Where to post it').setRequired(true).addChannelTypes(...PANEL_CHANNEL_TYPES))
    .addStringOption((o) => o.setName('title').setDescription('Panel title').setMaxLength(256))
    .addStringOption((o) => o.setName('type').setDescription('Role mode (default Normal)').addChoices(...TYPE_CHOICES))
    .addStringOption((o) => o.setName('layout').setDescription('Default Buttons').addChoices(...LAYOUT_CHOICES))
    .addStringOption((o) => o.setName('template').setDescription('Preset title, text and mode').addChoices(...TEMPLATE_CHOICES))
    .addStringOption((o) => o.setName('description').setDescription('Embed text under the title').setMaxLength(4000))
    .addStringOption((o) => o.setName('message').setDescription('Plain text sent with the panel').setMaxLength(2000))
    .addBooleanOption((o) => o.setName('plain_text').setDescription('Send text only, no embed (default False)'))
    .addIntegerOption((o) => o.setName('max_roles').setDescription('Most roles a member can hold (Normal only)').setMinValue(1).setMaxValue(25))
    .addRoleOption((o) => o.setName('role').setDescription('First role to add (the verified role for Verify)'))
    .addStringOption((o) => o.setName('emoji').setDescription('Emoji for that first role').setMaxLength(100))
    .addStringOption((o) => o.setName('placeholder').setDescription('Dropdown placeholder text').setMaxLength(150)))
  .addSubcommand((s) => s.setName('attach').setDescription('Turn an existing message into an emoji-reaction panel')
    .addStringOption((o) => o.setName('message_link').setDescription('Message link, or ID of a message in this channel').setRequired(true).setMaxLength(200))
    .addStringOption((o) => o.setName('type').setDescription('Role mode (default Normal)').addChoices(...TYPE_CHOICES))
    .addStringOption((o) => o.setName('title').setDescription('Name shown in lists').setMaxLength(100))
    .addIntegerOption((o) => o.setName('max_roles').setDescription('Most roles a member can hold (Normal only)').setMinValue(1).setMaxValue(20)))
  .addSubcommand((s) => s.setName('add').setDescription('Add a role to a panel')
    .addStringOption((o) => panelOption(o))
    .addRoleOption((o) => o.setName('role').setDescription('Role to offer').setRequired(true))
    .addStringOption((o) => o.setName('emoji').setDescription('Emoji (required on emoji panels)').setMaxLength(100))
    .addStringOption((o) => o.setName('label').setDescription('Text (default: role name)').setMaxLength(80))
    .addStringOption((o) => o.setName('style').setDescription('Button colour (default Blue)').addChoices(...STYLE_CHOICES))
    .addStringOption((o) => o.setName('description').setDescription('Dropdown option description').setMaxLength(100)))
  .addSubcommand((s) => s.setName('remove').setDescription('Remove a role from a panel (also deleted roles)')
    .addStringOption((o) => panelOption(o))
    .addStringOption((o) => o.setName('item').setDescription('Role to remove').setRequired(true).setAutocomplete(true).setMaxLength(100)))
  .addSubcommand((s) => s.setName('edit').setDescription('Edit a panel')
    .addStringOption((o) => panelOption(o))
    .addStringOption((o) => o.setName('title').setDescription('New title').setMaxLength(256))
    .addStringOption((o) => o.setName('description').setDescription('New embed text (- resets)').setMaxLength(4000))
    .addStringOption((o) => o.setName('message').setDescription('New plain text (- removes)').setMaxLength(2000))
    .addBooleanOption((o) => o.setName('plain_text').setDescription('Text only, no embed'))
    .addStringOption((o) => o.setName('type').setDescription('Role mode').addChoices(...TYPE_CHOICES))
    .addIntegerOption((o) => o.setName('max_roles').setDescription('Most roles a member can hold (0 = no limit)').setMinValue(0).setMaxValue(25))
    .addStringOption((o) => o.setName('placeholder').setDescription('New dropdown placeholder (- resets)').setMaxLength(150)))
  .addSubcommand((s) => s.setName('repost').setDescription('Post a panel again (deleted message) or move it')
    .addStringOption((o) => panelOption(o))
    .addChannelOption((o) => o.setName('channel').setDescription('Default: its current channel').addChannelTypes(...PANEL_CHANNEL_TYPES)))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a panel')
    .addStringOption((o) => panelOption(o)))
  .addSubcommand((s) => s.setName('list').setDescription('List role panels'));

// ── Messages ──────────────────────────────────────────────────────────────────

const ROLE_HINT = 'Pick a role below my highest role (and below yours), or move my role up in Server Settings → Roles.';
const NOT_FOUND = () => errorEmbed('Panel Not Found', "I can't find that panel. Pick one from the list as you type.");

const W = {
  module: "⚠️ The Reaction Roles module is off, so panels don't respond. Turn it on with `/module enable reactionroles`.",
  manageMessages: (channelId) => `⚠️ I don't have **Manage Messages** in <#${channelId}>, so I can't remove a member's old reaction when they pick a new role.`,
  missing: (title) => `⚠️ The panel message no longer exists. Run \`/reactionrole repost panel:${clampText(title, 60)}\` to post it again.`,
  sync: (reason) => `⚠️ I couldn't update the panel message: ${reason}.`,
  react: (emoji, reason) => `⚠️ I couldn't add the ${emoji} reaction: ${reason}.`,
  maxRoles: 'ℹ️ `max_roles` only applies to Normal panels, so it was ignored.',
  style: 'ℹ️ `style` only applies to button panels, so it was ignored.',
  description: 'ℹ️ `description` only applies to dropdown panels, so it was ignored.',
  placeholder: 'ℹ️ `placeholder` only applies to dropdown panels, so it was ignored.',
};

/** Appends warning lines after a blank line. */
const withWarnings = (text, warnings) => {
  const lines = warnings.filter(Boolean);
  return lines.length ? `${text}\n\n${lines.join('\n')}` : text;
};

/** The warning for a sync or fetch result that is not ok, or null. */
function statusWarning(result, title) {
  if (result?.status === 'missing') return W.missing(title);
  if (result?.status === 'failed') return W.sync(result.reason);
  return null;
}

const moduleWarning = (settings) => (settings && !isModuleEnabled(settings, 'reactionroles') ? W.module : null);
const lacksManageMessages = (channel, guild) => channel?.permissionsFor?.(guild.members.me)?.has(PermissionFlagsBits.ManageMessages) !== true;

/** Edits the (deferred) reply with one embed; falls back to a follow-up when the original was deleted. Never throws. */
async function respond(interaction, embed) {
  const payload = { embeds: [embed], components: [] };
  try {
    return await interaction.editReply(payload);
  } catch (err) {
    try {
      if (err?.code === 10008) return await interaction.followUp(payload);
    } catch (err2) {
      if (!isInteractionGone(err2)) logger.warn('reactionrole reply failed', err2);
      return null;
    }
    if (!isInteractionGone(err)) logger.warn('reactionrole reply failed', err);
    return null;
  }
}

const missingPermissions = (c, channel, check) => respond(c.interaction, check.missing.length
  ? errorEmbed('Missing Permissions', `I need **${permissionNames(check.missing).join(', ')}** in <#${channel.id}>.`)
  : errorEmbed('Invalid Channel', check.reason));

async function lookupPanel(c, panelId) {
  const group = await rr.getPanel(c.guildId, panelId);
  if (!group) await respond(c.interaction, NOT_FOUND());
  return group;
}

/**
 * Checks a role the admin wants on a panel (bot and executor hierarchy, managed roles) and asks before a role with
 * dangerous permissions is offered. Replies and returns null when the role cannot be used.
 */
async function checkRole(c, role) {
  const { interaction, guild } = c;
  const r = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: true });
  if (!r.ok) {
    await respond(interaction, errorEmbed('Role Not Allowed', `${r.reason} ${ROLE_HINT}`));
    return null;
  }
  if (r.dangerous.length > 0) {
    const ok = await awaitConfirm(interaction, {
      title: 'Dangerous role',
      description: `<@&${role.id}> has **${permissionNames(r.dangerous).join(', ')}**. Anyone who uses this panel gets those permissions. Add it anyway?`,
      confirmLabel: 'Add anyway',
      ephemeral: false,
    });
    if (!ok) {
      await respond(interaction, infoEmbed('Cancelled', 'Nothing was changed.'));
      return null;
    }
  }
  return role;
}

// ── /reactionrole create ──────────────────────────────────────────────────────

async function create(c) {
  const { interaction, client, guild, guildId, settings } = c;
  const o = interaction.options;
  const channelOpt = o.getChannel('channel', true);
  const typeOpt = o.getString('type');
  const layoutOpt = o.getString('layout');
  const templateKey = o.getString('template');
  const descriptionOpt = o.getString('description');
  const messageOpt = o.getString('message')?.trim() || null;
  const plainText = o.getBoolean('plain_text') ?? false;
  const maxRolesOpt = o.getInteger('max_roles');
  const roleOpt = o.getRole('role');
  const emojiOpt = o.getString('emoji');
  const placeholderOpt = o.getString('placeholder')?.trim() || null;
  const warnings = [];

  const tpl = templateKey && Object.hasOwn(rr.RR_TEMPLATES, templateKey) ? rr.RR_TEMPLATES[templateKey] : null;
  const type = typeOpt ?? tpl?.type ?? 'NORMAL';
  const layout = layoutOpt ?? tpl?.layout ?? 'BUTTONS';
  const title = o.getString('title')?.trim() || tpl?.title || rr.DEFAULT_TITLE;
  const description = descriptionOpt?.trim() || tpl?.description[layout] || null;
  const maxRoles = type === 'NORMAL' ? (maxRolesOpt ?? null) : null;
  if (maxRolesOpt !== null && type !== 'NORMAL') warnings.push(W.maxRoles);
  const placeholder = layout === 'SELECT' ? placeholderOpt : null;
  if (placeholderOpt && layout !== 'SELECT') warnings.push(W.placeholder);

  const channel = guild.channels.cache.get(channelOpt.id) ?? null;
  const needs = ['ViewChannel', 'SendMessages', ...(plainText ? [] : ['EmbedLinks']), ...(layout === 'REACTIONS' ? ['AddReactions', 'ReadMessageHistory'] : [])];
  const check = checkChannelUsable(channel, { needs });
  if (!check.ok) return missingPermissions(c, channelOpt, check);

  if ((await rr.countPanels(guildId)) >= LIMITS.REACTION_ROLE_PANELS) {
    return respond(interaction, errorEmbed('Panel Limit Reached', `This server already has ${LIMITS.REACTION_ROLE_PANELS} reaction-role panels. Delete one with \`/reactionrole delete\` first.`));
  }

  let item = null;
  let role = null;
  if (emojiOpt && !roleOpt) {
    return respond(interaction, errorEmbed('Missing Role', 'The `emoji` option sets the emoji for `role`. Pick a role too, or leave `emoji` empty.'));
  }
  if (roleOpt) {
    role = await checkRole(c, guild.roles.cache.get(roleOpt.id) ?? null);
    if (!role) return null;
    const stored = emojiOpt ?? tpl?.item.emoji ?? null;
    const pe = rr.parseEmojiInput(stored, { guild, client });
    if (!pe.ok) return respond(interaction, errorEmbed('Invalid Emoji', pe.error));
    if (layout === 'REACTIONS' && !pe.emoji) {
      return respond(interaction, errorEmbed('Emoji Needed', 'Emoji-reaction panels need an emoji for each role. Add `emoji:`.'));
    }
    item = {
      roleId: role.id,
      label: clampText(tpl?.item.label ?? role.name, 80),
      emoji: pe.emoji,
      style: layout === 'BUTTONS' ? (tpl?.item.style ?? 1) : 1,
    };
  }

  await ensureGuildSettings(guildId);
  const id = randomUUID();
  const draft = {
    id, guildId, channelId: channel.id, messageId: '0', title, description, type, layout, maxRoles, plainText,
    messageContent: messageOpt, attached: false, placeholder,
    items: item ? [rr.draftItem(id, item, 0)] : [],
  };

  let sent;
  try {
    sent = await channel.send(rr.buildPanelPayload(draft, { color: settings?.embedColor }));
  } catch (err) {
    return respond(interaction, errorEmbed('Could Not Post', `I couldn't post in <#${channel.id}>: ${rr.describeDiscordError(err)}.`));
  }
  try {
    await rr.createPanel({ ...draft, messageId: sent.id });
  } catch (err) {
    markBotDeletion(sent.id, 'reaction-roles:rollback');
    await sent.delete().catch(() => {});
    logger.error('reactionrole create failed to save the panel', err);
    return respond(interaction, errorEmbed('Something Went Wrong', 'I could not save the panel, so I removed the message. Please try again.'));
  }
  await rr.invalidatePanelIndex(guildId);

  if (layout === 'REACTIONS' && item) {
    const r = await rr.addPanelReaction(sent, item.emoji);
    if (!r.ok) warnings.push(W.react(item.emoji, r.reason));
  }
  if (type === 'UNIQUE' && layout === 'REACTIONS' && lacksManageMessages(channel, guild)) warnings.push(W.manageMessages(channel.id));

  const lines = [`Panel **${clampText(title, 100)}** posted in <#${channel.id}> · [Jump](${sent.url}).`];
  if (item) lines.push(`First role: <@&${item.roleId}>${item.emoji ? ' ' + item.emoji : ''}.`);
  lines.push(`Add ${item ? 'more ' : ''}roles with \`/reactionrole add\`.`);
  return respond(interaction, successEmbed('Panel Created', withWarnings(lines.join('\n'), [...warnings, moduleWarning(settings)])));
}

// ── /reactionrole attach ──────────────────────────────────────────────────────

async function attach(c) {
  const { interaction, guild, guildId, settings } = c;
  const o = interaction.options;
  const ref = rr.parseMessageReference(o.getString('message_link', true), { guildId, channelId: interaction.channelId });
  if (!ref.ok) return respond(interaction, errorEmbed('Invalid Message', ref.error));

  const channel = guild.channels.cache.get(ref.channelId) ?? await guild.channels.fetch(ref.channelId).catch(() => null);
  if (!channel || !channel.isTextBased()) {
    return respond(interaction, errorEmbed('Channel Not Found', "I can't find that channel, or it isn't a text channel."));
  }
  const check = checkChannelUsable(channel, { needs: ['ViewChannel', 'ReadMessageHistory', 'AddReactions'] });
  if (!check.ok) return missingPermissions(c, channel, check);

  let message;
  try {
    message = await channel.messages.fetch(ref.messageId);
  } catch (err) {
    const code = rr.discordErrorCode(err);
    if (code === 10008) return respond(interaction, errorEmbed('Message Not Found', "I can't find that message. Copy its link again (right-click → Copy Message Link)."));
    if (code === 50001 || code === 50013) return respond(interaction, errorEmbed('Missing Permissions', `I can't read messages in <#${channel.id}>.`));
    throw err;
  }

  const existing = await rr.findPanelByMessage(guildId, message.id);
  if (existing) {
    return respond(interaction, errorEmbed('Already a Panel', `That message is already the panel **${clampText(existing.title, 100)}**. Add roles to it with \`/reactionrole add\`.`));
  }
  if ((await rr.countPanels(guildId)) >= LIMITS.REACTION_ROLE_PANELS) {
    return respond(interaction, errorEmbed('Panel Limit Reached', `This server already has ${LIMITS.REACTION_ROLE_PANELS} reaction-role panels. Delete one with \`/reactionrole delete\` first.`));
  }
  if (message.reactions.cache.size >= rr.MAX_ITEMS.REACTIONS) {
    return respond(interaction, errorEmbed('Too Many Reactions', "That message already has 20 different reactions, so I can't add role emojis to it. Clear some reactions first."));
  }

  const warnings = [];
  const type = o.getString('type') ?? 'NORMAL';
  const maxRolesOpt = o.getInteger('max_roles');
  const maxRoles = type === 'NORMAL' ? (maxRolesOpt ?? null) : null;
  if (maxRolesOpt !== null && type !== 'NORMAL') warnings.push(W.maxRoles);
  const title = o.getString('title')?.trim() || rr.defaultAttachTitle(message, channel.name);

  await ensureGuildSettings(guildId);
  await rr.createPanel({
    id: randomUUID(), guildId, channelId: channel.id, messageId: message.id, title, description: null, type,
    layout: 'REACTIONS', maxRoles, plainText: false, messageContent: null, attached: true, items: [],
  });
  await rr.invalidatePanelIndex(guildId);

  if (type === 'UNIQUE' && lacksManageMessages(channel, guild)) warnings.push(W.manageMessages(channel.id));
  const text = `**${clampText(title, 100)}** is now an emoji-reaction panel on [this message](${message.url}). I won't change the message itself.\n`
    + `Add roles with \`/reactionrole add panel:${clampText(title, 60)} role:… emoji:…\` and I'll add each reaction.`;
  return respond(interaction, successEmbed('Panel Attached', withWarnings(text, [...warnings, moduleWarning(settings)])));
}

// ── /reactionrole add ─────────────────────────────────────────────────────────

async function add(c) {
  const { interaction, client, guild, guildId, settings } = c;
  const o = interaction.options;
  const group = await lookupPanel(c, o.getString('panel', true));
  if (!group) return null;
  const roleOpt = o.getRole('role', true);
  const role = guild.roles.cache.get(roleOpt.id) ?? null;
  const title = clampText(group.title, 100);

  if (role && group.items.some((i) => i.roleId === role.id)) {
    return respond(interaction, errorEmbed('Already Added', `<@&${role.id}> is already on **${title}**.`));
  }
  const maxItems = rr.MAX_ITEMS[group.layout];
  if (group.items.length >= maxItems) {
    return respond(interaction, errorEmbed('Panel Full', `A ${rr.LAYOUT_LABELS[group.layout].toLowerCase()} panel can hold at most ${maxItems} roles.`));
  }
  if (!(await checkRole(c, role))) return null;

  const pe = rr.parseEmojiInput(o.getString('emoji'), { guild, client });
  if (!pe.ok) return respond(interaction, errorEmbed('Invalid Emoji', pe.error));
  if (group.layout === 'REACTIONS') {
    if (!pe.emoji) return respond(interaction, errorEmbed('Emoji Needed', 'Emoji-reaction panels need an emoji for each role. Add `emoji:`.'));
    const other = group.items.find((i) => rr.emojiKeyOfStored(i.emoji) === pe.key);
    if (other) return respond(interaction, errorEmbed('Emoji In Use', `${pe.emoji} is already used for <@&${other.roleId}> on this panel.`));
  }

  const warnings = [];
  const label = clampText(o.getString('label')?.trim() || role.name, 80);
  const styleOpt = o.getString('style');
  const style = group.layout === 'BUTTONS' ? (rr.STYLE_MAP[styleOpt] ?? 1) : 1;
  if (styleOpt && group.layout !== 'BUTTONS') warnings.push(W.style);
  const descriptionOpt = o.getString('description')?.trim() || null;
  const description = descriptionOpt ? clampText(descriptionOpt, 100) : null;
  if (descriptionOpt && group.layout !== 'SELECT') warnings.push(W.description);

  let item;
  try {
    item = await rr.addItem(group, { roleId: role.id, label, emoji: pe.emoji, style, description });
  } catch (err) {
    if (err?.code === 'P2002') return respond(interaction, errorEmbed('Already Added', `<@&${role.id}> is already on **${title}**.`));
    throw err;
  }

  if (!group.attached) {
    const sync = await rr.syncPanel(guild, group.id, { settings });
    if (sync.status === 'failed' && sync.code === 50035) {
      await rr.removeItem(group, item.id);
      await rr.syncPanel(guild, group.id, { settings });
      await rr.invalidatePanelIndex(guildId);
      return respond(interaction, errorEmbed('Panel Update Failed', `Discord rejected the updated panel (${sync.reason}), so I undid the change.`));
    }
    warnings.push(statusWarning(sync, group.title));
  }
  await rr.invalidatePanelIndex(guildId);

  if (group.layout === 'REACTIONS') {
    const fm = await rr.fetchPanelMessage(guild, group);
    if (fm.status === 'ok') {
      const r = await rr.addPanelReaction(fm.message, pe.emoji);
      if (!r.ok) {
        if (r.code === 10014 || r.code === 30010) {
          await rr.removeItem(group, item.id);
          if (!group.attached) await rr.syncPanel(guild, group.id, { settings });
          await rr.invalidatePanelIndex(guildId);
          return respond(interaction, errorEmbed('Could Not Add Reaction', `${rr.describeDiscordError(r)}, so I undid the change.`));
        }
        warnings.push(W.react(pe.emoji, r.reason));
      }
    } else if (group.attached) {
      warnings.push(statusWarning(fm, group.title));
    }
  }

  return respond(interaction, successEmbed('Role Added', withWarnings(
    `<@&${role.id}> added to **${title}**${pe.emoji ? ` with ${pe.emoji}` : ''}.`,
    [...warnings, group.type === 'UNIQUE' && group.layout === 'REACTIONS' && lacksManageMessages(guild.channels.cache.get(group.channelId), guild) ? W.manageMessages(group.channelId) : null, moduleWarning(settings)],
  )));
}

// ── /reactionrole remove ──────────────────────────────────────────────────────

async function remove(c) {
  const { interaction, client, guild, guildId, settings } = c;
  const o = interaction.options;
  const group = await lookupPanel(c, o.getString('panel', true));
  if (!group) return null;
  const item = rr.resolveItem(group, o.getString('item', true));
  if (!item) return respond(interaction, errorEmbed('Not On Panel', "That role isn't on this panel. Pick one from the list as you type."));

  const warnings = [];
  const isReactions = group.layout === 'REACTIONS';
  if (isReactions) await rr.invalidatePanelIndex(guildId); // before the item goes, so our own hooks never see a half-removed one
  await rr.removeItem(group, item.id);
  if (!group.attached) warnings.push(statusWarning(await rr.syncPanel(guild, group.id, { settings }), group.title));
  if (isReactions) {
    const fm = await rr.fetchPanelMessage(guild, group);
    if (fm.status === 'ok') {
      if (item.emoji) await rr.removePanelReaction(fm.message, item.emoji, { client }); // cosmetic: failures are ignored
    } else if (group.attached) {
      warnings.push(statusWarning(fm, group.title));
    }
  }
  await rr.invalidatePanelIndex(guildId); // the index may have been reloaded between the two steps

  const who = guild.roles.cache.has(item.roleId) ? `<@&${item.roleId}>` : `**${clampText(item.label, 80)}** (deleted role)`;
  return respond(interaction, successEmbed('Role Removed', withWarnings(
    `${who} removed from **${clampText(group.title, 100)}**. Members who already have the role keep it.`,
    [...warnings, moduleWarning(settings)],
  )));
}

// ── /reactionrole edit ────────────────────────────────────────────────────────

/** '-' or an empty value resets a text field to null. */
const textOrNull = (value) => {
  const t = value.trim();
  return t === '-' || t === '' ? null : t;
};

async function edit(c) {
  const { interaction, guild, guildId, settings } = c;
  const o = interaction.options;
  const group = await lookupPanel(c, o.getString('panel', true));
  if (!group) return null;

  const titleOpt = o.getString('title');
  const descriptionOpt = o.getString('description');
  const messageOpt = o.getString('message');
  const plainOpt = o.getBoolean('plain_text');
  const typeOpt = o.getString('type');
  const maxRolesOpt = o.getInteger('max_roles');
  const placeholderOpt = o.getString('placeholder');
  if ([titleOpt, descriptionOpt, messageOpt, plainOpt, typeOpt, maxRolesOpt, placeholderOpt].every((v) => v === null)) {
    return respond(c.interaction, errorEmbed('Nothing To Change', 'Give at least one option to change.'));
  }
  if (group.attached && (descriptionOpt !== null || messageOpt !== null || plainOpt !== null)) {
    return respond(interaction, errorEmbed('Attached Panel', 'This panel uses an existing message that I never edit, so only `title`, `type` and `max_roles` can change.'));
  }

  const warnings = [];
  const changed = [];
  const patch = {};
  if (titleOpt !== null) { patch.title = titleOpt.trim() || rr.DEFAULT_TITLE; changed.push('title'); }
  if (descriptionOpt !== null) { patch.description = textOrNull(descriptionOpt); changed.push('description'); }
  if (messageOpt !== null) { patch.messageContent = textOrNull(messageOpt); changed.push('message'); }
  if (plainOpt !== null) { patch.plainText = plainOpt; changed.push('plain_text'); }
  if (typeOpt !== null) { patch.type = typeOpt; changed.push('type'); }
  if (placeholderOpt !== null) {
    if (group.layout === 'SELECT') { patch.placeholder = textOrNull(placeholderOpt); changed.push('placeholder'); } else warnings.push(W.placeholder);
  }
  if ((patch.type ?? group.type) !== 'NORMAL') {
    patch.maxRoles = null;
    if (maxRolesOpt !== null && maxRolesOpt !== 0) warnings.push(W.maxRoles);
  } else if (maxRolesOpt !== null) {
    patch.maxRoles = maxRolesOpt === 0 ? null : maxRolesOpt;
    changed.push('max_roles');
  }
  if (changed.length === 0) {
    return respond(interaction, errorEmbed('Nothing To Change', withWarnings('None of those options apply to this panel.', warnings)));
  }

  const updated = await rr.updatePanel(guildId, group.id, patch);
  if (!updated) return respond(interaction, NOT_FOUND());
  await rr.invalidatePanelIndex(guildId);
  if (!updated.attached) warnings.push(statusWarning(await rr.syncPanel(guild, updated, { settings }), updated.title));
  if (updated.type === 'UNIQUE' && updated.layout === 'REACTIONS' && lacksManageMessages(guild.channels.cache.get(updated.channelId), guild)) {
    warnings.push(W.manageMessages(updated.channelId));
  }

  return respond(interaction, successEmbed('Panel Updated', withWarnings(
    `**${clampText(updated.title, 100)}** updated: ${changed.join(', ')}.`,
    [...warnings, moduleWarning(settings)],
  )));
}

// ── /reactionrole repost ──────────────────────────────────────────────────────

async function repost(c) {
  const { interaction, client, guild, guildId, settings } = c;
  const o = interaction.options;
  const group = await lookupPanel(c, o.getString('panel', true));
  if (!group) return null;
  const channelOpt = o.getChannel('channel');
  const title = clampText(group.title, 100);
  const warnings = [];

  const target = (channelOpt ? guild.channels.cache.get(channelOpt.id) : null)
    ?? guild.channels.cache.get(group.channelId)
    ?? await guild.channels.fetch(group.channelId).catch(() => null);
  if (!target) return respond(interaction, errorEmbed('Channel Needed', "The panel's channel no longer exists. Run this again with `channel:`."));

  const restoreOnly = group.attached && !channelOpt;
  let oldMessage = null;
  if (restoreOnly) {
    const fm = await rr.fetchPanelMessage(guild, group);
    if (fm.status === 'ok') {
      const res = await rr.ensurePanelReactions(fm.message, rr.indexItems(group));
      const lines = [`I re-added the role reactions on [the attached message](${fm.message.url}).`];
      for (const f of res.failed.slice(0, 5)) lines.push(W.react(f.emoji, f.reason));
      return respond(interaction, successEmbed('Reactions Restored', lines.join('\n')));
    }
    if (fm.status === 'failed') {
      return respond(interaction, errorEmbed('Could Not Reach Panel', `I couldn't reach the attached message: ${fm.reason}.`));
    }
    // missing: the panel becomes a message the bot posts below
  }

  const needs = ['ViewChannel', 'SendMessages', ...(group.plainText ? [] : ['EmbedLinks']), ...(group.layout === 'REACTIONS' ? ['AddReactions', 'ReadMessageHistory'] : [])];
  const check = checkChannelUsable(target, { needs });
  if (!check.ok) return missingPermissions(c, target, check);

  // Look the old message up before the panel moves: a fetch that finds nothing flags the panel as missing.
  if (!restoreOnly) oldMessage = await rr.fetchPanelMessage(guild, group);

  let sent;
  try {
    sent = await target.send(rr.buildPanelPayload({ ...group, attached: false }, { color: settings?.embedColor }));
  } catch (err) {
    return respond(interaction, errorEmbed('Could Not Post', `I couldn't post in <#${target.id}>: ${rr.describeDiscordError(err)}.`));
  }
  try {
    await rr.movePanel(guildId, group.id, { channelId: target.id, messageId: sent.id, attached: false });
  } catch (err) {
    markBotDeletion(sent.id, 'reaction-roles:rollback');
    await sent.delete().catch(() => {});
    logger.error('reactionrole repost failed to save the panel', err);
    return respond(interaction, errorEmbed('Something Went Wrong', 'I could not save the new message, so I removed it. Please try again.'));
  }
  await rr.invalidatePanelIndex(guildId);
  if (group.layout === 'REACTIONS') {
    const res = await rr.ensurePanelReactions(sent, rr.indexItems(group));
    for (const f of res.failed.slice(0, 5)) warnings.push(W.react(f.emoji, f.reason));
  }

  // Clean up the old message (only when it is a different one).
  if (oldMessage?.status === 'ok' && oldMessage.message.id !== sent.id) {
    if (!group.attached) {
      markBotDeletion(oldMessage.message.id, 'reaction-roles:repost');
      await oldMessage.message.delete().catch(() => {});
    } else {
      await rr.removeOwnReactions(oldMessage.message, rr.indexItems(group), client); // its content is never touched
    }
  }

  let text = `**${title}** is now at [this message](${sent.url}) in <#${target.id}>.`;
  if (group.attached) text += " It's now a message I posted, so `/reactionrole edit` can change its text. The original message was left alone.";
  if (group.type === 'UNIQUE' && group.layout === 'REACTIONS' && lacksManageMessages(target, guild)) warnings.push(W.manageMessages(target.id));
  return respond(interaction, successEmbed('Panel Reposted', withWarnings(text, [...warnings, moduleWarning(settings)])));
}

// ── /reactionrole delete ──────────────────────────────────────────────────────

async function del(c) {
  const { interaction, client, guild, guildId } = c;
  const group = await lookupPanel(c, interaction.options.getString('panel', true));
  if (!group) return null;
  const title = clampText(group.title, 100);

  const ok = await awaitConfirm(interaction, {
    title: 'Delete this panel?',
    description: `**${title}** and its ${group.items.length} role(s) will be removed${group.attached ? '' : ", and I'll delete the panel message"}. Members keep the roles they already have.`,
    confirmLabel: 'Delete',
  });
  if (!ok) return respond(interaction, infoEmbed('Cancelled', 'The panel was not deleted.'));

  await rr.deletePanel(guildId, group.id);
  await rr.invalidatePanelIndex(guildId);

  let messageDeleted = false;
  const fm = await rr.fetchPanelMessage(guild, group);
  if (fm.status === 'ok') {
    if (!group.attached) {
      markBotDeletion(fm.message.id, 'reaction-roles:delete');
      messageDeleted = await fm.message.delete().then(() => true, () => false);
    } else {
      await rr.removeOwnReactions(fm.message, rr.indexItems(group), client); // never delete or edit an attached message
    }
  }

  const text = group.attached
    ? `**${title}** was deleted. The original message was left alone.`
    : messageDeleted
      ? `**${title}** was deleted, along with its message.`
      : `**${title}** was deleted. Its message was already gone, or I couldn't delete it.`;
  try {
    await interaction.followUp({ embeds: [successEmbed('Panel Deleted', text)] });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('reactionrole delete follow-up failed', err);
  }
  return null;
}

// ── /reactionrole list ────────────────────────────────────────────────────────

/**
 * One embed per page of 10 panels. Pure; every field is clamped so a page stays far below Discord's 6000-character limit.
 * @param {Array<object>} panels rows of listPanels()
 * @param {{ guildId: string, total?: number, settings?: object|null }} opts
 * @returns {import('discord.js').EmbedBuilder[]}
 */
export function buildListPages(panels, { guildId, total = panels.length, settings = null } = {}) {
  const pages = [];
  for (let start = 0; start < panels.length; start += rr.LIST_PAGE_SIZE) {
    const embed = applyGuildColor(infoEmbed(`Reaction Role Panels (${total})`), settings);
    panels.slice(start, start + rr.LIST_PAGE_SIZE).forEach((p, i) => {
      const lines = [
        `${rr.TYPE_LABELS[p.type] ?? p.type} · ${rr.LAYOUT_LABELS[p.layout] ?? p.layout} · ${p.itemCount} role(s)${p.maxRoles ? ` · max ${p.maxRoles}` : ''}`,
        `<#${p.channelId}> · [Jump](https://discord.com/channels/${guildId}/${p.channelId}/${p.messageId})${p.attached ? ' · attached' : ''}${p.plainText ? ' · text only' : ''}`,
      ];
      if (p.messageMissingAt) lines.push('⚠️ message deleted — run /reactionrole repost');
      embed.addFields({ name: `${start + i + 1}. ${clampText(p.title, 200)}`, value: clampText(lines.join('\n'), 1024) });
    });
    pages.push(embed);
  }
  return pages;
}

async function list(c) {
  const { interaction, guildId, settings } = c;
  const panels = await rr.listPanels(guildId);
  if (panels.length === 0) {
    return respond(interaction, infoEmbed('No Panels', 'This server has no reaction-role panels yet. Create one with `/reactionrole create`, or use `/reactionrole attach` on an existing message.'));
  }
  await paginate(interaction, buildListPages(panels, { guildId, total: panels.length, settings }), { timeout: 120_000 });
  return null;
}

// ── Entry points ──────────────────────────────────────────────────────────────

const HANDLERS = { create, attach, add, remove, edit, repost, delete: del, list };

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand();
  const handler = Object.hasOwn(HANDLERS, sub) ? HANDLERS[sub] : null;
  if (!handler) return;
  // Public replies (admin convention); the delete prompt is private and its result is a public follow-up.
  if (!(await safeDefer(interaction, { ephemeral: sub === 'delete' }))) return;
  try {
    const guild = interaction.guild;
    const guildId = interaction.guildId;
    const settings = await getSettingsSafe(guildId); // may be null: the command still works
    await handler({ interaction, client, guild, guildId, settings });
  } catch (err) {
    logger.error(`reactionrole ${sub} failed`, err);
    await respond(interaction, errorEmbed('Something Went Wrong', 'I could not save that change. Please try again in a moment.'));
  }
}

export async function autocomplete(interaction) {
  try {
    const guildId = interaction.guildId;
    const focused = interaction.options.getFocused(true);
    const q = String(focused.value ?? '').trim().slice(0, 100);
    if (!guildId) return await respondAutocomplete(interaction, []);

    if (focused.name === 'panel') {
      const rows = await rr.searchPanels(guildId, q);
      return await respondAutocomplete(interaction, rows.map((row) => ({ name: rr.panelChoiceName(row), value: row.id })));
    }
    if (focused.name === 'item') {
      const panelId = interaction.options.getString('panel');
      if (!panelId) return await respondAutocomplete(interaction, []);
      const roleIds = q
        ? [...interaction.guild.roles.cache.filter((r) => r.name.toLowerCase().includes(q.toLowerCase())).keys()].slice(0, 100)
        : [];
      const items = await rr.searchItems(guildId, panelId, q, roleIds);
      return await respondAutocomplete(interaction, items.map((item) => ({ name: rr.itemChoiceName(item, interaction.guild), value: item.id })));
    }
    return await respondAutocomplete(interaction, []);
  } catch (err) {
    logger.debug(`reactionrole autocomplete failed: ${err?.message ?? err}`);
    return respondAutocomplete(interaction, []);
  }
}
