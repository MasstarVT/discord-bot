// /ticket-panel (pkg-tickets): MEE6 Ticketing setup. Thin: option parsing, validation and replies. Everything else (rendering,
// the writes under advisory locks, publishing, the open flow) lives in services/ticketService.js. Replies are public (admin
// convention) except `message`, which opens a form and therefore must not defer first.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId } from '../../config/customIds.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  DEFAULT_INTRO_TEMPLATE, OPEN_REQUIRED_PERMS, addOption, buildCategoriesEmbed, buildPanelDetailsEmbed, buildTranscriptsEmbed,
  categoriesText, clampTemplateForModal, createPanel, defaultPanelTemplate, deletePanel, describeDiscordError as describe, ensureCategory, findOptionsForAutocomplete,
  findPanel, findPanelsForAutocomplete, liveManagerRoleIds, listPanelsView, normalizePanelName, optionLine, panelErrorPayload,
  panelListLine, parseOptionEmoji, peekPanel, publishPanel, refreshNote, refreshPanelMessage, removeOption, renderChannelName,
  syncManagerOverwrites, templateUsesToken, transcriptsText, updateManagers, updateOption, updatePanelFields, validateNameFormat,
  MANAGERS_WARNING,
} from '../../services/ticketService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { buildTemplateModal, normalizeTemplate } from '../../utils/customEmbed.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  awaitConfirm, checkChannelUsable, isInteractionGone, missingBotPermissions, permissionNames, pingableRoleIds, respondAutocomplete,
  safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { chunkLines, clampText } from '../../utils/text.js';

export const module = 'tickets';
export const gated = false; // configuration keeps working while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

// ── Builder ───────────────────────────────────────────────────────────────────

const panelOpt = (o) => o.setName('panel').setDescription('Ticket panel').setRequired(true).setAutocomplete(true);
const styleOpt = (o) => o.setName('style').setDescription('Open with buttons or a dropdown (default buttons)')
  .addChoices({ name: 'Buttons', value: 'BUTTONS' }, { name: 'Dropdown', value: 'SELECT' });
const textCh = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const colorOpt = (o) => o.setName('color').setDescription('Button colour (default Blue)')
  .addChoices({ name: 'Blue', value: 1 }, { name: 'Grey', value: 2 }, { name: 'Green', value: 3 }, { name: 'Red', value: 4 });

export const data = new SlashCommandBuilder()
  .setName('ticket-panel')
  .setDescription('Set up ticket panels: options, managers, categories, transcripts (MEE6 Ticketing).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('create').setDescription('Create a panel with a default "Open Ticket" button.')
    .addStringOption((o) => o.setName('name').setDescription('Panel name (unique on this server)').setRequired(true).setMinLength(1).setMaxLength(50))
    .addChannelOption((o) => o.setName('channel').setDescription('Publish it here right away').addChannelTypes(...textCh))
    .addStringOption((o) => newChannelOption(o))
    .addStringOption(styleOpt)
    .addBooleanOption((o) => o.setName('ask_reason').setDescription('Ask for a reason in a form before opening'))
    .addIntegerOption((o) => o.setName('max_open').setDescription('Open tickets per member on this panel (default 1)').setMinValue(1).setMaxValue(10))
    .addStringOption((o) => o.setName('name_format').setDescription('Channel name, e.g. ticket-{number} or {user.name}-{number}').setMaxLength(90)))
  .addSubcommand((s) => s.setName('edit').setDescription('Change panel settings; with no options, show them.')
    .addStringOption(panelOpt)
    .addStringOption((o) => o.setName('name').setDescription('New panel name').setMinLength(1).setMaxLength(50))
    .addStringOption(styleOpt)
    .addBooleanOption((o) => o.setName('ask_reason').setDescription('Ask for a reason in a form before opening'))
    .addIntegerOption((o) => o.setName('max_open').setDescription('Open tickets per member on this panel').setMinValue(1).setMaxValue(10))
    .addStringOption((o) => o.setName('name_format').setDescription('Channel name format ({number} {user.name} {user.id} {topic})').setMaxLength(90)))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a panel and its message (open tickets stay).')
    .addStringOption(panelOpt))
  .addSubcommand((s) => s.setName('list').setDescription('List the ticket panels on this server.'))
  .addSubcommand((s) => s.setName('publish').setDescription('Post or update the panel message in a channel.')
    .addStringOption(panelOpt)
    .addChannelOption((o) => o.setName('channel').setDescription('Channel to post the panel in').addChannelTypes(...textCh))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('message').setDescription('Edit the panel message or the ticket intro message in a form.')
    .addStringOption(panelOpt)
    .addStringOption((o) => o.setName('type').setDescription('Which message (default panel)')
      .addChoices({ name: 'Panel message', value: 'panel' }, { name: 'Ticket intro', value: 'intro' }))
    .addStringOption((o) => o.setName('option').setDescription('Intro for one option only (type intro)').setAutocomplete(true)))
  .addSubcommand((s) => s.setName('categories').setDescription('Set the open/claimed/closed categories; with no options, show them.')
    .addStringOption(panelOpt)
    .addChannelOption((o) => o.setName('open').setDescription('Category for new tickets').addChannelTypes(ChannelType.GuildCategory))
    .addStringOption((o) => newChannelOption(o, 'open', 'category'))
    .addChannelOption((o) => o.setName('claimed').setDescription('Category for claimed tickets').addChannelTypes(ChannelType.GuildCategory))
    .addStringOption((o) => newChannelOption(o, 'claimed', 'category'))
    .addChannelOption((o) => o.setName('closed').setDescription('Category for closed tickets').addChannelTypes(ChannelType.GuildCategory))
    .addStringOption((o) => newChannelOption(o, 'closed', 'category'))
    .addBooleanOption((o) => o.setName('create_missing').setDescription('Create any category that is not set (MEE6 "Create one for me")')))
  .addSubcommand((s) => s.setName('transcripts').setDescription('Where transcripts go when a ticket is deleted; no options = show.')
    .addStringOption(panelOpt)
    .addChannelOption((o) => o.setName('channel').setDescription('Channel that receives transcripts').addChannelTypes(...textCh))
    .addStringOption((o) => newChannelOption(o))
    .addBooleanOption((o) => o.setName('dm_owner').setDescription('Also DM the transcript to the ticket opener'))
    .addBooleanOption((o) => o.setName('disable').setDescription('Stop posting transcripts to a channel')))
  .addSubcommandGroup((g) => g.setName('option').setDescription('Panel buttons / dropdown options')
    .addSubcommand((s) => s.setName('add').setDescription('Add a button or dropdown option (max 25).')
      .addStringOption(panelOpt)
      .addStringOption((o) => o.setName('label').setDescription('Label, e.g. Open Ticket').setRequired(true).setMinLength(1).setMaxLength(80))
      .addStringOption((o) => o.setName('emoji').setDescription('Emoji, e.g. ✉️ or a server emoji').setMaxLength(64))
      .addStringOption((o) => o.setName('description').setDescription('Dropdown description').setMaxLength(100))
      .addIntegerOption(colorOpt)
      .addChannelOption((o) => o.setName('category').setDescription('Open tickets of this option here').addChannelTypes(ChannelType.GuildCategory))
      .addStringOption((o) => newChannelOption(o, 'category', 'category')))
    .addSubcommand((s) => s.setName('edit').setDescription('Change an option.')
      .addStringOption(panelOpt)
      .addStringOption((o) => o.setName('option').setDescription('Option to change').setRequired(true).setAutocomplete(true))
      .addStringOption((o) => o.setName('label').setDescription('New label').setMinLength(1).setMaxLength(80))
      .addStringOption((o) => o.setName('emoji').setDescription('New emoji ("none" removes it)').setMaxLength(64))
      .addStringOption((o) => o.setName('description').setDescription('New dropdown description ("none" removes it)').setMaxLength(100))
      .addIntegerOption(colorOpt)
      .addChannelOption((o) => o.setName('category').setDescription('Open tickets of this option here').addChannelTypes(ChannelType.GuildCategory))
      .addStringOption((o) => newChannelOption(o, 'category', 'category'))
      .addBooleanOption((o) => o.setName('clear_category').setDescription('Use the panel open category again')))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove an option (a panel keeps at least one).')
      .addStringOption(panelOpt)
      .addStringOption((o) => o.setName('option').setDescription('Option to remove').setRequired(true).setAutocomplete(true)))
    .addSubcommand((s) => s.setName('list').setDescription('List the options of a panel.')
      .addStringOption(panelOpt)))
  .addSubcommandGroup((g) => g.setName('managers').setDescription('Ticket Manager roles (max 10 per panel)')
    .addSubcommand((s) => s.setName('add').setDescription("Let a role see and handle this panel's tickets.")
      .addStringOption(panelOpt)
      .addRoleOption((o) => o.setName('role').setDescription('Role'))
      .addStringOption((o) => newRoleOption(o)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a Ticket Manager role.')
      .addStringOption(panelOpt)
      .addRoleOption((o) => o.setName('role').setDescription('Role').setRequired(true))));

// Per subcommand for resolveNewOptions: pickers that need a value, and channels created hidden from @everyone.
const NEW_TARGETS = Object.freeze({
  publish: { required: ['channel'] },
  transcripts: { private: ['channel'] },
  'managers add': { required: ['role'] },
});

// ── Shared helpers ────────────────────────────────────────────────────────────

const NO_PINGS = { parse: [] };
const CATEGORY_PERMS = ['ViewChannel', 'ManageChannels', 'ManageRoles'];
const styleLabel = (style) => (style === 'SELECT' ? 'Dropdown' : 'Buttons');
const yesNo = (v) => (v ? 'Yes' : 'No');
const loadSettings = async (guildId) => peekSettings(guildId) ?? getSettingsSafe(guildId);

// An option-resolved channel can be a raw API object without permissionsFor: prefer the cached one.
const cached = (guild, picked) => (picked ? (guild.channels.cache.get(picked.id) ?? picked) : null);

/** The bot's permissions in a category, as a P18 payload (or null when fine). */
function categoryProblem(category) {
  const missing = missingBotPermissions(category, CATEGORY_PERMS);
  return missing.length ? panelErrorPayload('CATEGORY_PERMS', { names: permissionNames(missing).join(', '), category: category.name }) : null;
}

async function resolvePanel(ctx) {
  const panel = await findPanel(ctx.guild.id, ctx.interaction.options.getString('panel', true));
  if (!panel) {
    await ctx.reply(panelErrorPayload('PANEL_NOT_FOUND'));
    return null;
  }
  return panel;
}

const findOption = (panel, ref) => panel.options.find((o) => o.id === ref || o.label.toLowerCase() === String(ref).toLowerCase()) ?? null;

// The publish pre-checks: the channel, and the server-wide permissions every ticket needs (a panel whose button fails at once is
// worse than no panel). Returns `{ channel }` or `{ error }` (a payload).
function publishChecks(guild, picked) {
  const channel = cached(guild, picked);
  const u = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
  if (!u.ok) return { error: panelErrorPayload('CHANNEL_UNUSABLE', { u, id: channel?.id ?? picked.id }) };
  const missing = missingBotPermissions(guild, OPEN_REQUIRED_PERMS);
  if (missing.length) return { error: panelErrorPayload('PERMS_SERVER', { missing }) };
  return { channel };
}

// The warnings that follow a successful publish: no manager roles, and an open category the bot cannot use.
function publishWarnings(guild, panel) {
  const lines = [];
  if (liveManagerRoleIds(panel, guild).length === 0) lines.push(MANAGERS_WARNING);
  const open = panel.openCategoryId ? guild.channels.cache.get(panel.openCategoryId) : null;
  if (open && missingBotPermissions(open, CATEGORY_PERMS).length) {
    lines.push(`⚠️ I can't manage channels in the **${open.name}** category. Tickets would fail to open there.`);
  }
  return lines;
}

// ── execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (!group && sub === 'message') return message(interaction);

  if (!(await safeDefer(interaction))) return undefined;
  if (!(await resolveNewOptions(interaction, data, NEW_TARGETS[group ? `${group} ${sub}` : sub]))) return undefined;
  const ctx = {
    interaction, client, guild: interaction.guild, settings: await loadSettings(interaction.guildId),
    reply: (payload) => safeReply(interaction, payload),
  };
  switch (group ? `${group} ${sub}` : sub) {
    case 'create': return create(ctx);
    case 'edit': return edit(ctx);
    case 'delete': return remove(ctx);
    case 'list': return list(ctx);
    case 'publish': return publish(ctx);
    case 'categories': return categories(ctx);
    case 'transcripts': return transcripts(ctx);
    case 'option add': return optionAdd(ctx);
    case 'option edit': return optionEdit(ctx);
    case 'option remove': return optionRemove(ctx);
    case 'option list': return optionList(ctx);
    case 'managers add': return managersChange(ctx, 'add');
    case 'managers remove': return managersChange(ctx, 'remove');
    default: return undefined;
  }
}

// ── create / edit / delete / list / publish ───────────────────────────────────

async function create(ctx) {
  const { interaction, guild, reply, settings } = ctx;
  const o = interaction.options;
  const name = normalizePanelName(o.getString('name', true));
  if (!name) return reply(panelErrorPayload('BAD_PANEL_NAME'));
  let format = null;
  const formatInput = o.getString('name_format');
  if (formatInput !== null) {
    const v = validateNameFormat(formatInput);
    if (!v.ok) return reply(panelErrorPayload('BAD_FORMAT', { error: v.error }));
    format = v.format;
  }

  const r = await createPanel({
    guildId: guild.id,
    name,
    style: o.getString('style') ?? 'BUTTONS',
    askReason: o.getBoolean('ask_reason') ?? false,
    maxOpenPerUser: o.getInteger('max_open') ?? 1,
    channelNameFormat: format ?? 'ticket-{number}',
  });
  if (!r.ok) return reply(panelErrorPayload(r.code, r.data));
  const { panel } = r;

  const steps = [
    `Created **${name}** (${styleLabel(panel.style)}) with an **Open Ticket** ✉️ option.`,
    'Next steps:',
    '• `/ticket-panel managers add` — who handles the tickets',
    '• `/ticket-panel option add` — more buttons or dropdown options',
    '• `/ticket-panel message` — the panel and intro texts',
  ];
  const picked = o.getChannel('channel');
  if (picked) {
    const checks = publishChecks(guild, picked);
    let line;
    if (checks.error) {
      line = `⚠️ The panel was created but not published: ${checks.error.embeds[0].data.description}\n• Publish it later with \`/ticket-panel publish\``;
    } else {
      const p = await publishPanel({ guild, panelId: panel.id, channel: checks.channel, settings });
      line = p.ok
        ? `Published it in <#${checks.channel.id}>. [Jump to panel](${p.message.url})`
        : `⚠️ The panel was created but I couldn't post it in <#${checks.channel.id}> (${p.code === 'PANEL_GONE' ? 'the panel was deleted' : describe(p.error)}).\n• Publish it later with \`/ticket-panel publish\``;
    }
    steps.push(line);
  } else {
    steps.push('• `/ticket-panel publish` — post the panel');
  }
  steps.push(MANAGERS_WARNING); // a new panel never has manager roles
  return reply({ embeds: [successEmbed('Panel Created', steps.join('\n'))], allowedMentions: NO_PINGS });
}

async function edit(ctx) {
  const { interaction, guild, reply, settings } = ctx;
  const o = interaction.options;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;

  const nameIn = o.getString('name');
  const style = o.getString('style');
  const askReason = o.getBoolean('ask_reason');
  const maxOpen = o.getInteger('max_open');
  const formatIn = o.getString('name_format');
  if ([nameIn, style, askReason, maxOpen, formatIn].every((v) => v === null)) {
    return reply({ embeds: [buildPanelDetailsEmbed(panel, guild, settings)], allowedMentions: NO_PINGS });
  }

  const data = {};
  const lines = [];
  if (nameIn !== null) {
    const name = normalizePanelName(nameIn);
    if (!name) return reply(panelErrorPayload('BAD_PANEL_NAME'));
    data.name = name;
    lines.push(`Name: **${panel.name}** → **${name}**`);
  }
  if (style !== null) {
    data.style = style;
    lines.push(`Style: **${styleLabel(style)}**`);
  }
  if (askReason !== null) {
    data.askReason = askReason;
    lines.push(`Ask for a reason: **${yesNo(askReason)}**`);
  }
  if (maxOpen !== null) {
    data.maxOpenPerUser = maxOpen;
    lines.push(`Open tickets per member: **${maxOpen}**`);
  }
  if (formatIn !== null) {
    const v = validateNameFormat(formatIn);
    if (!v.ok) return reply(panelErrorPayload('BAD_FORMAT', { error: v.error }));
    data.channelNameFormat = v.format;
    const preview = renderChannelName(v.format, { number: 1, user: interaction.user, member: interaction.member, topic: panel.options[0]?.label ?? '' });
    lines.push(`Channel name: \`${v.format}\` — preview \`${preview}\``);
  }

  const r = await updatePanelFields(panel.id, guild.id, data);
  if (!r.ok) return reply(panelErrorPayload(r.code === 'PANEL_NOT_FOUND' ? 'PANEL_NOT_FOUND' : 'NAME_TAKEN', r.data));
  const refresh = data.style && data.style !== panel.style ? await refreshPanelMessage(guild, panel.id) : null;
  return reply({ embeds: [successEmbed('Panel Updated', lines.join('\n') + refreshNote(refresh))], allowedMentions: NO_PINGS });
}

async function remove(ctx) {
  const { interaction, guild, reply } = ctx;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;
  const ok = await awaitConfirm(interaction, {
    title: 'Delete Panel?',
    description: `Delete **${clampText(panel.name, 80)}** and its published message? Open tickets keep their channels.`,
    confirmLabel: 'Delete',
    ephemeral: false,
  });
  if (!ok) return reply({ embeds: [infoEmbed('Cancelled', 'The panel was not deleted.')], components: [] });
  const { open } = await deletePanel(guild, panel);
  const text = `Deleted **${clampText(panel.name, 80)}**.${open ? ` ${open} ticket(s) keep their channels; only moderators and admins can manage them now.` : ''}`;
  return reply({ embeds: [successEmbed('Panel Deleted', text)], components: [], allowedMentions: NO_PINGS });
}

async function list(ctx) {
  const { interaction, guild, reply, settings } = ctx;
  const rows = await listPanelsView(guild);
  if (rows.length === 0) return reply({ embeds: [infoEmbed('No Ticket Panels', 'Create one with `/ticket-panel create`.')] });
  const pages = chunkLines(rows.map((r) => panelListLine(r, guild)), 4000)
    .map((chunk) => guildEmbed(settings, `🎫 Ticket panels (${rows.length}/${LIMITS.TICKET_PANELS})`, chunk));
  return paginate(interaction, pages);
}

async function publish(ctx) {
  const { interaction, guild, reply, settings } = ctx;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;
  const checks = publishChecks(guild, interaction.options.getChannel('channel', true));
  if (checks.error) return reply(checks.error);

  const r = await publishPanel({ guild, panelId: panel.id, channel: checks.channel, settings });
  if (!r.ok) return reply(panelErrorPayload('PUBLISH_FAILED', { reason: r.code === 'PANEL_GONE' ? 'the panel was deleted' : describe(r.error) }));
  const edited = r.mode === 'edited';
  const lines = [`${edited ? 'Updated' : 'Posted'} **${clampText(panel.name, 80)}** in <#${checks.channel.id}>. [Jump to panel](${r.message.url})`, ...publishWarnings(guild, panel)];
  return reply({ embeds: [successEmbed(edited ? 'Panel Updated' : 'Panel Published', lines.join('\n'))], allowedMentions: NO_PINGS });
}

// ── message (the template form; no defer: showModal must be the first response) ──

async function message(interaction) {
  const o = interaction.options;
  const deny = (payload) => safeReply(interaction, { ...payload, ephemeral: true });
  const type = o.getString('type') ?? 'panel';
  const optionRef = o.getString('option');
  if (type === 'panel' && optionRef) return deny(panelErrorPayload('BAD_COMBO'));

  // The lookup shares the 3 s acknowledgement window: memo first, then a bounded read.
  const ref = o.getString('panel', true);
  const budget = ackBudgetMs(interaction);
  let panel = peekPanel(interaction.guildId, ref);
  if (!panel) {
    panel = budget > 0
      ? await withTimeout(findPanel(interaction.guildId, ref), budget, 'ticket-panel message lookup').catch(() => undefined)
      : undefined;
  }
  if (panel === undefined) return deny(panelErrorPayload('SLOW'));
  if (panel === null) return deny(panelErrorPayload('PANEL_NOT_FOUND'));
  let option = null;
  if (optionRef) {
    option = findOption(panel, optionRef);
    if (!option) return deny(panelErrorPayload('OPTION_NOT_FOUND'));
  }

  let base;
  if (type === 'panel') base = panel.template ? normalizeTemplate(panel.template) : defaultPanelTemplate(panel.style);
  else base = normalizeTemplate(option?.introTemplate ?? panel.introTemplate ?? DEFAULT_INTRO_TEMPLATE);

  const customId = type === 'panel'
    ? buildCustomId('ticket-modal', 'tpl', panel.id, 'panel')
    : buildCustomId('ticket-modal', 'tpl', panel.id, 'intro', ...(option ? [option.id] : []));
  let title;
  if (type === 'panel') title = `Panel message: ${panel.name}`;
  else title = option ? `Intro: ${option.label}` : `Ticket intro: ${panel.name}`;

  try {
    await interaction.showModal(buildTemplateModal({ customId, title: clampText(title, 45), template: clampTemplateForModal(base) }));
  } catch (err) {
    if (!isInteractionGone(err)) logger.error('ticket-panel message: showModal failed', err);
  }
  return undefined;
}

// ── categories / transcripts ──────────────────────────────────────────────────

async function categories(ctx) {
  const { interaction, guild, reply, settings } = ctx;
  const o = interaction.options;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;

  const picked = {
    openCategoryId: cached(guild, o.getChannel('open')),
    claimedCategoryId: cached(guild, o.getChannel('claimed')),
    closedCategoryId: cached(guild, o.getChannel('closed')),
  };
  const createMissing = o.getBoolean('create_missing') ?? false;
  const given = Object.entries(picked).filter(([, c]) => c);
  if (given.length === 0 && !createMissing) {
    return reply({ embeds: [buildCategoriesEmbed(panel, guild, settings)], allowedMentions: NO_PINGS });
  }

  for (const [, category] of given) {
    const problem = categoryProblem(category);
    if (problem) return reply(problem);
  }
  const data = Object.fromEntries(given.map(([field, category]) => [field, category.id]));
  let fresh = panel;
  if (given.length) {
    const r = await updatePanelFields(panel.id, guild.id, data);
    if (!r.ok) return reply(panelErrorPayload('PANEL_NOT_FOUND'));
    fresh = { ...panel, ...data };
  }

  const extra = [];
  if (createMissing) {
    const kinds = [['open', 'openCategoryId'], ['claimed', 'claimedCategoryId'], ['closed', 'closedCategoryId']];
    for (const [kind, field] of kinds) {
      if (fresh[field] && guild.channels.cache.has(fresh[field])) continue;
      const e = await ensureCategory(guild, panel.id, kind);
      if (e.created) extra.push(`Created **${e.category.name}**.`);
      else if (e.reused && e.category) extra.push(`Using the existing **${e.category.name}** category.`);
      if (e.note) extra.push(`⚠️ ${e.note}`);
    }
    fresh = (await findPanel(guild.id, panel.id)) ?? fresh;
  }

  const full = given.filter(([, c]) => guild.channels.cache.filter((ch) => ch.parentId === c.id).size >= 50)
    .map(([, c]) => `⚠️ **${c.name}** is full (50 channels). New tickets will be created outside any category until there is room.`);
  const lines = [categoriesText(fresh, guild), ...extra, ...full, 'Existing tickets are not moved.'];
  return reply({ embeds: [successEmbed('Categories Updated', lines.join('\n'))], allowedMentions: NO_PINGS });
}

async function transcripts(ctx) {
  const { interaction, guild, reply, settings } = ctx;
  const o = interaction.options;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;

  const channel = cached(guild, o.getChannel('channel'));
  const dmOwner = o.getBoolean('dm_owner');
  const disable = o.getBoolean('disable');
  if (!channel && dmOwner === null && disable === null) {
    return reply({ embeds: [buildTranscriptsEmbed(panel, settings)], allowedMentions: NO_PINGS });
  }
  if (channel && disable === true) return reply(panelErrorPayload('CONFLICT_CHANNEL'));

  if (channel) {
    const u = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles'] });
    if (!u.ok) return reply(panelErrorPayload('TRANSCRIPT_CHANNEL_UNUSABLE', { u, id: channel.id }));
  }
  const data = {};
  if (channel) data.transcriptChannelId = channel.id;
  if (disable === true) data.transcriptChannelId = null;
  if (dmOwner !== null) data.dmTranscript = dmOwner;
  if (Object.keys(data).length === 0) return reply({ embeds: [buildTranscriptsEmbed(panel, settings)], allowedMentions: NO_PINGS });

  const r = await updatePanelFields(panel.id, guild.id, data);
  if (!r.ok) return reply(panelErrorPayload('PANEL_NOT_FOUND'));
  return reply({ embeds: [successEmbed('Transcripts Updated', transcriptsText({ ...panel, ...data }))], allowedMentions: NO_PINGS });
}

// ── option add | edit | remove | list ─────────────────────────────────────────

async function optionAdd(ctx) {
  const { interaction, guild, client, reply } = ctx;
  const o = interaction.options;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;

  const label = o.getString('label', true).trim();
  if (label.length < 1 || label.length > 80) return reply(panelErrorPayload('BAD_LABEL'));
  const e = parseOptionEmoji(o.getString('emoji') ?? '', { client, guild });
  if (!e.ok) return reply(panelErrorPayload('BAD_EMOJI', { error: e.error }));
  const category = cached(guild, o.getChannel('category'));
  if (category) {
    const problem = categoryProblem(category);
    if (problem) return reply(problem);
  }

  const r = await addOption(panel.id, guild.id, {
    label,
    emoji: e.emoji,
    description: o.getString('description')?.trim() || null,
    buttonStyle: o.getInteger('color') ?? 1,
    categoryId: category?.id ?? null,
  });
  if (!r.ok) return reply(panelErrorPayload(r.code, r.data));
  const refresh = await refreshPanelMessage(guild, panel.id);
  const text = `Added ${e.emoji ? `${e.emoji} ` : ''}**${clampText(label, 80)}** to **${clampText(panel.name, 80)}** (${r.count}/${LIMITS.TICKET_PANEL_OPTIONS}).`;
  return reply({ embeds: [successEmbed('Option Added', text + refreshNote(refresh))], allowedMentions: NO_PINGS });
}

async function optionEdit(ctx) {
  const { interaction, guild, client, reply, settings } = ctx;
  const o = interaction.options;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;
  const option = findOption(panel, o.getString('option', true));
  if (!option) return reply(panelErrorPayload('OPTION_NOT_FOUND'));

  const data = {};
  const labelIn = o.getString('label');
  if (labelIn !== null) {
    const label = labelIn.trim();
    if (label.length < 1 || label.length > 80) return reply(panelErrorPayload('BAD_LABEL'));
    data.label = label;
  }
  const emojiIn = o.getString('emoji');
  if (emojiIn !== null) {
    const e = parseOptionEmoji(emojiIn, { client, guild });
    if (!e.ok) return reply(panelErrorPayload('BAD_EMOJI', { error: e.error }));
    data.emoji = e.emoji;
  }
  const descIn = o.getString('description');
  if (descIn !== null) data.description = descIn.trim().toLowerCase() === 'none' ? null : (descIn.trim() || null);
  const color = o.getInteger('color');
  if (color !== null) data.buttonStyle = color;
  const category = cached(guild, o.getChannel('category'));
  const clear = o.getBoolean('clear_category');
  if (category && clear === true) return reply(panelErrorPayload('CONFLICT_CATEGORY'));
  if (category) {
    const problem = categoryProblem(category);
    if (problem) return reply(problem);
    data.categoryId = category.id;
  }
  if (clear === true) data.categoryId = null;

  if (Object.keys(data).length === 0) {
    const index = panel.options.findIndex((x) => x.id === option.id);
    return reply({ embeds: [guildEmbed(settings, `🎫 ${clampText(panel.name, 100)} — option`, optionLine(option, index, panel))], allowedMentions: NO_PINGS });
  }
  const r = await updateOption(panel.id, guild.id, option.id, data);
  if (!r.ok) return reply(panelErrorPayload(r.code, r.data));
  const refresh = await refreshPanelMessage(guild, panel.id);
  const updated = { ...option, ...data };
  const index = panel.options.findIndex((x) => x.id === option.id);
  return reply({ embeds: [successEmbed('Option Updated', optionLine(updated, index, panel) + refreshNote(refresh))], allowedMentions: NO_PINGS });
}

async function optionRemove(ctx) {
  const { interaction, guild, reply } = ctx;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;
  const option = findOption(panel, interaction.options.getString('option', true));
  if (!option) return reply(panelErrorPayload('OPTION_NOT_FOUND'));
  const r = await removeOption(panel.id, guild.id, option.id);
  if (!r.ok) return reply(panelErrorPayload(r.code, r.data));
  const refresh = await refreshPanelMessage(guild, panel.id);
  return reply({ embeds: [successEmbed('Option Removed', `Removed **${clampText(option.label, 80)}**.${refreshNote(refresh)}`)], allowedMentions: NO_PINGS });
}

async function optionList(ctx) {
  const { reply, settings } = ctx;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;
  const lines = panel.options.map((option, i) => optionLine(option, i, panel));
  return reply({
    embeds: [guildEmbed(settings, `🎫 ${clampText(panel.name, 100)} — options (${panel.options.length}/${LIMITS.TICKET_PANEL_OPTIONS})`, lines.join('\n'))],
    allowedMentions: NO_PINGS,
  });
}

// ── managers add | remove ─────────────────────────────────────────────────────

async function managersChange(ctx, mode) {
  const { interaction, guild, reply } = ctx;
  const panel = await resolvePanel(ctx);
  if (!panel) return undefined;
  const role = interaction.options.getRole('role', true);
  if (mode === 'add' && role.id === guild.id) return reply(panelErrorPayload('EVERYONE_ROLE'));

  // Configuration time: when an intro pings {managers}, the new role must be pingable by the admin (00-overview rule 23).
  if (mode === 'add') {
    const intros = [panel.introTemplate, ...panel.options.map((x) => x.introTemplate)].filter(Boolean);
    const where = guild.channels.cache.get(interaction.channelId) ?? interaction.channel;
    if (where && intros.some((t) => templateUsesToken(t, 'managers'))) {
      const { refused } = pingableRoleIds(where, interaction.member, [role.id]);
      if (refused.length) {
        return reply({
          embeds: [errorEmbed('Role Not Pingable', `<@&${role.id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#${where.id}>. A ticket intro of this panel pings \`{managers}\`.`)],
          allowedMentions: NO_PINGS,
        });
      }
    }
  }

  const r = await updateManagers(panel.id, guild, (current) => {
    if (mode === 'add') {
      if (current.includes(role.id)) return { ok: false, code: 'ALREADY_MANAGER', data: { roleId: role.id, panel: panel.name } };
      if (current.length >= LIMITS.TICKET_MANAGER_ROLES) return { ok: false, code: 'MANAGER_LIMIT' };
      return { ok: true, roles: [...current, role.id] };
    }
    if (!current.includes(role.id)) return { ok: false, code: 'NOT_MANAGER', data: { roleId: role.id, panel: panel.name } };
    return { ok: true, roles: current.filter((id) => id !== role.id) };
  });
  if (!r.ok) return reply(panelErrorPayload(r.code, r.data));

  const sync = await syncManagerOverwrites({ guild, panelId: panel.id, roleId: role.id, mode });
  const list = r.roles.length ? r.roles.map((id) => `<@&${id}>`).join(' ') : 'none';
  const lines = [
    `<@&${role.id}> ${mode === 'add' ? 'can now' : 'can no longer'} see and handle tickets from **${clampText(panel.name, 80)}**.`,
    `Updated ${sync.updated} existing ticket(s)${sync.failed ? `, ${sync.failed} failed (missing permissions)` : ''}${sync.skipped ? `, ${sync.skipped} not updated (too many — only the 50 newest are changed)` : ''}.`,
    `Manager roles: ${list} (${r.roles.length}/${LIMITS.TICKET_MANAGER_ROLES})`,
  ];
  return reply({ embeds: [successEmbed('Ticket Managers Updated', lines.join('\n'))], allowedMentions: NO_PINGS });
}

// ── autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    const q = String(focused.value ?? '').trim();
    let choices = [];
    if (focused.name === 'panel') {
      const rows = await findPanelsForAutocomplete(interaction.guildId, q);
      choices = rows.map((p) => ({ name: `${p.name} · ${p.style === 'SELECT' ? 'dropdown' : 'buttons'}`, value: p.id }));
    } else if (focused.name === 'option') {
      const panelRef = interaction.options.getString('panel') ?? '';
      if (panelRef) {
        const rows = await findOptionsForAutocomplete(interaction.guildId, panelRef, q);
        choices = rows.map((x) => ({ name: `${x.emoji && !x.emoji.startsWith('<') ? `${x.emoji} ` : ''}${x.label}`, value: x.id }));
      }
    }
    await respondAutocomplete(interaction, choices);
  } catch (err) {
    logger.warn('ticket-panel autocomplete failed', err);
    await respondAutocomplete(interaction, []);
  }
}
