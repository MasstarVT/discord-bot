// /embed (pkg-embeds): saved embed builder (fields, link buttons), publish to a channel, edit the live message later,
// and the Welcome Channel presets. Thin: data access, validation and rendering live in services/embedService.js.
import { AttachmentBuilder, ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { LIMITS as GUILD_LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId } from '../../config/customIds.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import {
  EmbedEditError,
  IMPORT_MAX_BYTES,
  addButton,
  addField,
  checkImageUrl,
  checkLinkUrl,
  collectUnknownTokens,
  countSavedEmbeds,
  createSavedEmbed,
  deleteSavedEmbedAndMessage,
  editField,
  getSavedEmbed,
  isClearWord,
  listSavedEmbeds,
  mutateSavedEmbed,
  normalizeEmbedName,
  parseButtonEmoji,
  parseImportPayload,
  publishHint,
  publishSavedEmbed,
  removeButton,
  removeField,
  renderSavedEmbed,
  replaceTemplate,
  requireSavedEmbed,
  searchSavedEmbeds,
  setAuthor,
  setColor,
  setFooter,
  setImages,
  setTitleUrl,
  setUpWelcomeChannel,
  unknownVariablesNote,
  updatePublishedEmbed,
} from '../../services/embedService.js';
import {
  LIMITS as TEMPLATE_LIMITS,
  buildTemplateModal,
  emptyTemplate,
  formatColor,
  normalizeTemplate,
  parseColor,
  templateToJson,
} from '../../utils/customEmbed.js';
import { respondAutocomplete, awaitConfirm, isInteractionGone, permissionNames } from '../../utils/interactionHelpers.js';
import { memberChannelProblem } from '../../utils/privacyGuards.js';
import { clampText } from '../../utils/text.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import { TimeoutError, ackBudgetMs, withTimeout } from '../../utils/async.js';
import { SafeFetchError, safeFetchText } from '../../utils/safeFetch.js';
import { placeholderHelp } from '../../utils/placeholderParser.js';
import logger from '../../utils/logger.js';

export const module = 'embeds';
export const gated = true; // the whole command is refused while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const TEXT_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.PublicThread,
  ChannelType.PrivateThread, ChannelType.AnnouncementThread];
const nameOpt = (o) => o.setName('name').setDescription('Saved embed name').setRequired(true).setAutocomplete(true).setMaxLength(32);
const indexOpt = (what) => (o) => o.setName('index').setDescription(`${what} number`).setRequired(true)
  .setMinValue(1).setMaxValue(25).setAutocomplete(true);
const posOpt = (o) => o.setName('position').setDescription('Position (1 = first; default: last)').setMinValue(1).setMaxValue(25);
const urlOpt = (n, d) => (o) => o.setName(n).setDescription(d).setMaxLength(2048);

export const data = new SlashCommandBuilder()
  .setName('embed')
  .setDescription('Embed Messages: build, publish and edit saved embeds (fields, link buttons)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('create').setDescription('Create a saved embed and open the editor form')
    .addStringOption((o) => o.setName('name').setDescription('New embed name (letters, numbers, - and _)').setRequired(true).setMaxLength(32)))
  .addSubcommand((s) => s.setName('edit').setDescription('Edit the message text, title, description, colour and footer in a form')
    .addStringOption(nameOpt))
  .addSubcommandGroup((g) => g.setName('field').setDescription('Embed fields (up to 25)')
    .addSubcommand((s) => s.setName('add').setDescription('Add a field')
      .addStringOption(nameOpt)
      .addStringOption((o) => o.setName('title').setDescription('Field title').setRequired(true).setMaxLength(256))
      .addStringOption((o) => o.setName('value').setDescription('Field text').setRequired(true).setMaxLength(1024))
      .addBooleanOption((o) => o.setName('inline').setDescription('Show next to other inline fields'))
      .addIntegerOption(posOpt))
    .addSubcommand((s) => s.setName('edit').setDescription('Change a field')
      .addStringOption(nameOpt)
      .addIntegerOption(indexOpt('Field'))
      .addStringOption((o) => o.setName('title').setDescription('New title').setMaxLength(256))
      .addStringOption((o) => o.setName('value').setDescription('New text').setMaxLength(1024))
      .addBooleanOption((o) => o.setName('inline').setDescription('Show next to other inline fields'))
      .addIntegerOption(posOpt))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a field')
      .addStringOption(nameOpt).addIntegerOption(indexOpt('Field')))
    .addSubcommand((s) => s.setName('list').setDescription('List the fields of an embed')
      .addStringOption(nameOpt)))
  .addSubcommandGroup((g) => g.setName('button').setDescription('Link buttons (up to 25)')
    .addSubcommand((s) => s.setName('add').setDescription('Add a link button')
      .addStringOption(nameOpt)
      .addStringOption((o) => o.setName('label').setDescription('Button text').setRequired(true).setMaxLength(80))
      .addStringOption((o) => o.setName('url').setDescription('Link (https://…)').setRequired(true).setMaxLength(512))
      .addStringOption((o) => o.setName('emoji').setDescription('Emoji shown on the button').setMaxLength(64))
      .addIntegerOption(posOpt))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a link button')
      .addStringOption(nameOpt).addIntegerOption(indexOpt('Button'))))
  .addSubcommand((s) => s.setName('image').setDescription('Set the large image and/or thumbnail (none = remove)')
    .addStringOption(nameOpt)
    .addStringOption(urlOpt('image_url', 'Large image URL, {user.avatar} or {server.icon}'))
    .addStringOption(urlOpt('thumbnail_url', 'Thumbnail URL, {user.avatar} or {server.icon}')))
  .addSubcommand((s) => s.setName('author').setDescription('Set the author line (text: none = remove)')
    .addStringOption(nameOpt)
    .addStringOption((o) => o.setName('text').setDescription('Author name').setMaxLength(256))
    .addStringOption(urlOpt('icon_url', 'Author icon URL'))
    .addStringOption(urlOpt('url', 'Link on the author name')))
  .addSubcommand((s) => s.setName('footer').setDescription('Set the footer and timestamp (text: none = remove)')
    .addStringOption(nameOpt)
    .addStringOption((o) => o.setName('text').setDescription('Footer text').setMaxLength(2048))
    .addStringOption(urlOpt('icon_url', 'Footer icon URL'))
    .addBooleanOption((o) => o.setName('timestamp').setDescription('Show the time the message was posted')))
  .addSubcommand((s) => s.setName('color').setDescription('Set the side colour (none = server default)')
    .addStringOption(nameOpt)
    .addStringOption((o) => o.setName('color').setDescription('Hex like #5865F2 or a colour name').setRequired(true).setMaxLength(16)))
  .addSubcommand((s) => s.setName('title-url').setDescription('Make the title a link (none = remove)')
    .addStringOption(nameOpt)
    .addStringOption((o) => o.setName('url').setDescription('Link (https://…) or none').setRequired(true).setMaxLength(2048)))
  .addSubcommand((s) => s.setName('preview').setDescription('Preview a saved embed (only you can see it)')
    .addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('publish').setDescription('Post a saved embed in a channel')
    .addStringOption(nameOpt)
    .addChannelOption((o) => o.setName('channel').setDescription('Channel to post in').setRequired(true).addChannelTypes(...TEXT_TYPES)))
  .addSubcommand((s) => s.setName('update').setDescription('Apply your changes to the published message')
    .addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('list').setDescription('List the saved embeds of this server'))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a saved embed')
    .addStringOption(nameOpt)
    .addBooleanOption((o) => o.setName('delete_message').setDescription('Also delete the published message (default: no)')))
  .addSubcommand((s) => s.setName('export').setDescription('Download a saved embed as a JSON file')
    .addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('import').setDescription('Create or replace a saved embed from JSON')
    .addStringOption((o) => o.setName('name').setDescription('Embed name (letters, numbers, - and _)').setRequired(true).setMaxLength(32))
    .addAttachmentOption((o) => o.setName('file').setDescription('JSON file (max 20 KB)'))
    .addStringOption((o) => o.setName('json').setDescription('JSON text (instead of a file)').setMaxLength(6000))
    .addBooleanOption((o) => o.setName('overwrite').setDescription('Replace the embed if the name exists (default: no)')))
  .addSubcommand((s) => s.setName('welcome-channel').setDescription('MEE6 Welcome Channel: post greeting, rules, links and staff embeds')
    .addBooleanOption((o) => o.setName('create').setDescription('Create a read-only #welcome channel (default: yes)'))
    .addChannelOption((o) => o.setName('channel').setDescription('Post in this existing channel instead')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)));

// ── Reply texts (error catalogue, pkg-embeds.md §4.6) ──────────────────────────

const E_NAME = () => errorEmbed('Invalid Name', 'Embed names can use letters, numbers, - and _ (up to 32 characters).');
const E_NOTFOUND = (name) => errorEmbed('Embed Not Found', `There is no saved embed called **${name}**. See \`/embed list\`.`);
const E_TAKEN = (name, extra = '') => errorEmbed('Name Taken', `An embed called **${name}** already exists. Edit it with \`/embed edit name:${name}\`.${extra}`);
const E_LIMIT = () => errorEmbed('Limit Reached', `This server already has ${GUILD_LIMITS.SAVED_EMBEDS} saved embeds, the maximum. Delete one with \`/embed delete\` first.`);
const E_NOTHING = () => errorEmbed('Nothing To Change', 'Give at least one of the optional values.');
const E_DISCORD = (err) => errorEmbed('Discord Rejected It', clampText(`Discord refused the message: ${err?.message ?? 'unknown error'}`, 1000));
const E_DB = () => errorEmbed('Something Went Wrong', 'I could not save that right now. Please try again in a moment.');
const E_EMPTY = () => errorEmbed('Nothing To Show', 'This embed renders empty. Add a title, description, field or content.');
const E_SLOW = () => errorEmbed('Try Again', 'The database is slow right now. Please try again in a moment.');

function embedForError(err) {
  switch (err.code) {
    case 'NOTFOUND': return errorEmbed('Embed Not Found', err.message);
    case 'TAKEN': return errorEmbed('Name Taken', err.message);
    case 'LIMIT': return errorEmbed('Limit Reached', err.message);
    default: return errorEmbed('Could Not Save', err.message);
  }
}

// Answers in the way that fits the interaction's state, tolerating an expired interaction.
async function sendError(interaction, embed) {
  try {
    if (!interaction.deferred && !interaction.replied) await interaction.reply({ embeds: [embed], ephemeral: true });
    else if (interaction.deferred && !interaction.replied) await interaction.editReply({ embeds: [embed], components: [], files: [] });
    else await interaction.followUp({ embeds: [embed], ephemeral: true });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('embeds: could not send an error reply', err);
  }
}

const renderCtxOf = (interaction) => ({
  member: interaction.member, user: interaction.user, guild: interaction.guild, channel: interaction.channel,
});

/** @returns {string|null} the normalised `name` option, or null (the E-NAME reply was sent) */
async function nameOrReply(interaction) {
  const name = normalizeEmbedName(interaction.options.getString('name', true));
  if (!name) await sendError(interaction, E_NAME());
  return name;
}

// The line every successful template change ends with: where to apply it, and unknown variables.
async function replyChanged(interaction, saved, title, text) {
  const tpl = normalizeTemplate(saved.template);
  const note = publishHint(saved, saved.name) + unknownVariablesNote(tpl, renderCtxOf(interaction));
  await interaction.editReply({ embeds: [successEmbed(title, `${text}${note}`)] });
}

const clampPosition = (position, length) => Math.min(Math.max(position, 1), length);

// ── Subcommands ────────────────────────────────────────────────────────────────

// Pre-modal checks must finish inside the 3 s acknowledgement window.
async function precheck(interaction, promise) {
  try {
    return await withTimeout(promise, ackBudgetMs(interaction), 'embed precheck');
  } catch (err) {
    if (err instanceof TimeoutError) {
      await sendError(interaction, E_SLOW());
      return undefined;
    }
    throw err;
  }
}

async function createEmbed(interaction) {
  const name = normalizeEmbedName(interaction.options.getString('name', true));
  if (!name) return sendError(interaction, E_NAME());
  const guildId = interaction.guildId;
  const checked = await precheck(interaction, Promise.all([getSavedEmbed(guildId, name), countSavedEmbeds(guildId)]));
  if (!checked) return undefined;
  const [existing, count] = checked;
  if (existing) return sendError(interaction, E_TAKEN(name));
  if (count >= GUILD_LIMITS.SAVED_EMBEDS) return sendError(interaction, E_LIMIT());
  // Nothing is written until the form is submitted: a closed form leaves no row.
  return interaction.showModal(buildTemplateModal({
    customId: buildCustomId('embed-modal', 'new', name),
    title: `New embed: ${name}`,
    template: emptyTemplate(),
  }));
}

const FORM_LABELS = { content: 'message text', title: 'title', description: 'description', color: 'colour', footer: 'footer' };

async function editEmbed(interaction) {
  const name = normalizeEmbedName(interaction.options.getString('name', true));
  if (!name) return sendError(interaction, E_NAME());
  const row = await precheck(interaction, getSavedEmbed(interaction.guildId, name));
  if (row === undefined) return undefined;
  if (!row) return sendError(interaction, E_NOTFOUND(name));
  let modal;
  try {
    modal = buildTemplateModal({
      customId: buildCustomId('embed-modal', row.id),
      title: `Edit embed: ${row.name}`,
      template: normalizeTemplate(row.template),
      overflow: 'throw',
    });
  } catch (err) {
    if (!(err instanceof RangeError)) throw err;
    const parts = (err.fields ?? []).map((f) => FORM_LABELS[f] ?? f).join(', ');
    return sendError(interaction, errorEmbed(
      'Too Long For The Form',
      `This embed has a field longer than the form allows (${parts}). Edit it with /embed field or /embed author instead, `
      + 'or use `/embed export`, edit the JSON and `/embed import` with `overwrite:true`.',
    ));
  }
  return interaction.showModal(modal);
}

async function fieldAdd(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const position = o.getInteger('position');
  let pos = 0;
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => {
    const n = tpl.embed?.fields?.length ?? 0;
    pos = position == null ? n + 1 : clampPosition(position, n + 1);
    return addField(tpl, {
      title: o.getString('title', true), value: o.getString('value', true), inline: o.getBoolean('inline') ?? false, position,
    });
  });
  await replyChanged(interaction, saved, 'Field Added', `Added field ${pos} to **${name}**.`);
}

async function fieldEdit(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const index = o.getInteger('index', true);
  const title = o.getString('title') ?? undefined;
  const value = o.getString('value') ?? undefined;
  const inline = o.getBoolean('inline') ?? undefined;
  const position = o.getInteger('position') ?? undefined;
  if (title === undefined && value === undefined && inline === undefined && position === undefined) {
    return sendError(interaction, E_NOTHING());
  }
  let newPos = index;
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => {
    const n = tpl.embed?.fields?.length ?? 0;
    newPos = position === undefined ? index : clampPosition(position, n);
    return editField(tpl, index, { title, value, inline, position });
  });
  return replyChanged(interaction, saved, 'Field Updated', `Updated field ${newPos} of **${name}**.`);
}

async function fieldRemove(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const index = interaction.options.getInteger('index', true);
  let oldTitle = '';
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => {
    oldTitle = tpl.embed?.fields?.[index - 1]?.name ?? '';
    return removeField(tpl, index);
  });
  return replyChanged(interaction, saved, 'Field Removed', `Removed field ${index} ("${clampText(oldTitle, 60)}") from **${name}**.`);
}

async function fieldList(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const row = await requireSavedEmbed(interaction.guildId, name);
  const settings = await getSettingsSafe(interaction.guildId);
  const fields = normalizeTemplate(row.template).embed?.fields ?? [];
  const body = fields.length
    ? clampText(fields.map((f, i) => `**${i + 1}.** ${clampText(f.name, 100)}${f.inline ? ' · inline' : ''}\n${clampText(f.value, 150)}`).join('\n\n'), 4096)
    : 'This embed has no fields. Add one with `/embed field add`.';
  await interaction.editReply({
    embeds: [guildEmbed(settings, `Fields of ${name}`, body, `${fields.length}/${TEMPLATE_LIMITS.fields} fields`)],
  });
}

async function buttonAdd(interaction, client) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const emojiRaw = o.getString('emoji');
  const emoji = emojiRaw == null ? null : parseButtonEmoji(emojiRaw, (id) => client.emojis.cache.has(id));
  const position = o.getInteger('position');
  let pos = 0;
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => {
    const n = tpl.buttons.length;
    pos = position == null ? n + 1 : clampPosition(position, n + 1);
    return addButton(tpl, { label: o.getString('label', true), url: o.getString('url', true), emoji, position });
  });
  await replyChanged(interaction, saved, 'Button Added', `Added button ${pos} to **${name}**.`);
}

async function buttonRemove(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const index = interaction.options.getInteger('index', true);
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => removeButton(tpl, index));
  await replyChanged(interaction, saved, 'Button Removed', `Removed button ${index} from **${name}**.`);
}

async function setImage(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const imageRaw = o.getString('image_url');
  const thumbRaw = o.getString('thumbnail_url');
  if (imageRaw == null && thumbRaw == null) return sendError(interaction, E_NOTHING());
  const pick = (raw) => (raw == null ? undefined : isClearWord(raw) ? null : checkImageUrl(raw));
  const image = pick(imageRaw);
  const thumbnail = pick(thumbRaw);
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => setImages(tpl, { image, thumbnail }));
  return replyChanged(interaction, saved, 'Images Updated', `Updated the images of **${name}**.`);
}

async function setAuthorLine(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const text = o.getString('text');
  const iconRaw = o.getString('icon_url');
  const urlRaw = o.getString('url');
  if (text == null && iconRaw == null && urlRaw == null) return sendError(interaction, E_NOTHING());
  const iconUrl = iconRaw == null ? undefined : isClearWord(iconRaw) ? null : checkImageUrl(iconRaw);
  const url = urlRaw == null ? undefined : isClearWord(urlRaw) ? null : checkLinkUrl(urlRaw, 2048);
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => setAuthor(tpl, { text: text ?? undefined, iconUrl, url }));
  const removed = text != null && isClearWord(text);
  return replyChanged(interaction, saved, removed ? 'Author Removed' : 'Author Updated',
    removed ? `Removed the author line of **${name}**.` : `Updated the author line of **${name}**.`);
}

async function setFooterLine(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const text = o.getString('text');
  const iconRaw = o.getString('icon_url');
  const timestamp = o.getBoolean('timestamp');
  if (text == null && iconRaw == null && timestamp == null) return sendError(interaction, E_NOTHING());
  const iconUrl = iconRaw == null ? undefined : isClearWord(iconRaw) ? null : checkImageUrl(iconRaw);
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => setFooter(tpl, {
    text: text ?? undefined, iconUrl, timestamp: timestamp ?? undefined,
  }));
  const removed = text != null && isClearWord(text);
  const stamp = timestamp == null ? '' : ` The timestamp is ${timestamp ? 'on' : 'off'}.`;
  return replyChanged(interaction, saved, removed ? 'Footer Removed' : 'Footer Updated',
    `${removed ? `Removed the footer of **${name}**.` : `Updated the footer of **${name}**.`}${stamp}`);
}

async function setSideColor(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const v = interaction.options.getString('color', true).trim();
  const clear = isClearWord(v) || v.toLowerCase() === 'default';
  const color = clear ? null : parseColor(v);
  if (!clear && color === null) {
    return sendError(interaction, errorEmbed(
      'Unknown Colour',
      'Use a hex code like #5865F2, or one of: red, green, blue, blurple, yellow, orange, purple, pink, white, black, grey.',
    ));
  }
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => setColor(tpl, color));
  return replyChanged(interaction, saved, 'Colour Updated',
    `**${name}** now uses ${color == null ? 'the server default colour' : formatColor(color)}.`);
}

async function setTitleLink(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const raw = interaction.options.getString('url', true);
  const url = isClearWord(raw) ? null : checkLinkUrl(raw, 2048);
  const saved = await mutateSavedEmbed(interaction.guildId, name, (tpl) => setTitleUrl(tpl, url));
  const hasTitle = Boolean(normalizeTemplate(saved.template).embed?.title);
  return replyChanged(interaction, saved, 'Title Link Updated',
    `${url ? `The title of **${name}** now links to ${clampText(url, 200)}.` : `Removed the title link of **${name}**.`}`
    + `${url && !hasTitle ? '\nNote: the link only shows when the embed has a title.' : ''}`);
}

async function previewEmbed(interaction) {
  await interaction.deferReply({ ephemeral: true });
  const name = await nameOrReply(interaction);
  if (!name) return;
  const row = await requireSavedEmbed(interaction.guildId, name);
  const settings = await getSettingsSafe(interaction.guildId);
  const tpl = normalizeTemplate(row.template);
  const payload = renderSavedEmbed(tpl, { member: interaction.member, guild: interaction.guild, channel: interaction.channel, settings }, { mentions: 'none' });
  if (!payload) return sendError(interaction, E_EMPTY());
  await interaction.editReply(payload);
  const unknown = collectUnknownTokens(tpl, renderCtxOf(interaction));
  if (unknown.length) {
    await interaction.followUp({
      embeds: [warnEmbed('Unknown Variables', `These variables are shown as typed: ${unknown.map((t) => `\`${t}\``).join(', ')}\n\n${clampText(placeholderHelp(['server', 'channel']), 1500)}`)],
      ephemeral: true,
    }).catch((err) => { if (!isInteractionGone(err)) logger.warn('embeds: unknown-variables follow-up failed', err); });
  }
}

const rolesNote = (refused, channel) => (refused?.length
  ? `\n⚠️ These roles will not be pinged (not mentionable, and you lack **Mention @everyone, @here and All Roles** in <#${channel.id}>): ${refused.map((id) => `<@&${id}>`).join(', ')}`
  : '');

async function publishEmbed(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const row = await requireSavedEmbed(interaction.guildId, name);
  const channel = interaction.options.getChannel('channel', true);
  const settings = await getSettingsSafe(interaction.guildId);
  const result = await publishSavedEmbed({ row, channel, member: interaction.member, settings });
  if (result.ok) {
    const prev = result.previous
      ? `\nThe previous copy in <#${result.previous.channelId}> is no longer tracked; it will not change on /embed update.`
      : '';
    const tpl = normalizeTemplate(row.template);
    await interaction.editReply({
      embeds: [successEmbed('Embed Published', `Posted **${name}** in ${channel}. [Jump to message](${result.message.url})${prev}${rolesNote(result.refused, channel)}${unknownVariablesNote(tpl, { ...renderCtxOf(interaction), channel })}`)],
    });
    return;
  }
  switch (result.code) {
    case 'MEMBER_NO_ACCESS':
      return sendError(interaction, errorEmbed('No Access', `You need **View Channel** and **Send Messages** in ${channel} to post there.`));
    case 'CHANNEL':
      return sendError(interaction, errorEmbed(
        'Missing Permissions',
        result.missing?.length ? `I need **${permissionNames(result.missing).join(', ')}** in ${channel} to post there.` : (result.reason ?? `I can't post in ${channel}.`),
      ));
    case 'EMPTY':
      return sendError(interaction, E_EMPTY());
    default:
      return sendError(interaction, E_DISCORD(result.error));
  }
}

async function updateEmbed(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const row = await requireSavedEmbed(interaction.guildId, name);
  const settings = await getSettingsSafe(interaction.guildId);
  const result = await updatePublishedEmbed({ row, guild: interaction.guild, member: interaction.member, settings });
  if (result.ok) {
    await interaction.editReply({
      embeds: [successEmbed('Embed Updated', `Updated the published **${name}** in <#${result.channel.id}>. [Jump](${result.message.url})`)],
    });
    return;
  }
  const channelId = row.channelId;
  switch (result.code) {
    case 'MEMBER_NO_ACCESS':
      return sendError(interaction, errorEmbed('No Access', `You need **View Channel** in <#${channelId}> to edit the message published there.`));
    case 'NOT_PUBLISHED':
      return sendError(interaction, warnEmbed(
        'Not Published',
        `**${name}** isn't published. Post it with \`/embed publish\`.${(result.row ?? row).channelId ? ` (It was last posted in <#${(result.row ?? row).channelId}>, but that message was deleted.)` : ''}`,
      ));
    case 'CHANNEL_GONE':
      return sendError(interaction, warnEmbed('Channel Deleted', `The channel **${name}** was posted in no longer exists, so it is a draft again. Post it with \`/embed publish\`.`));
    case 'MESSAGE_GONE':
      return sendError(interaction, warnEmbed('Message Deleted', `The published message was deleted, so **${name}** is a draft again. Post it with \`/embed publish\`.`));
    case 'NO_ACCESS':
      return sendError(interaction, errorEmbed(
        'Missing Permissions',
        `I need **${result.missing?.length ? permissionNames(result.missing).join(', ') : 'View Channel, Read Message History, Embed Links'}** in <#${channelId}> to edit the message.`,
      ));
    case 'ARCHIVED':
      return sendError(interaction, errorEmbed('Thread Archived', 'That thread is archived or locked. Unarchive it and try again.'));
    case 'EMPTY':
      return sendError(interaction, E_EMPTY());
    case 'UNAVAILABLE':
      return sendError(interaction, errorEmbed('Server Unavailable', 'Discord reports this server as temporarily unavailable. Please try again in a moment.'));
    default:
      return sendError(interaction, E_DISCORD(result.error));
  }
}

async function listEmbeds(interaction) {
  await interaction.deferReply();
  const guildId = interaction.guildId;
  const [rows, settings] = await Promise.all([listSavedEmbeds(guildId), getSettingsSafe(guildId)]);
  if (!rows.length) {
    await interaction.editReply({
      embeds: [infoEmbed('No Saved Embeds', 'Create one with `/embed create`, or post the Welcome Channel presets with `/embed welcome-channel`.')],
    });
    return;
  }
  const lines = rows.map((r) => {
    const sec = Math.floor(r.updatedAt.getTime() / 1000);
    return r.messageId && r.channelId
      ? `\`${r.name}\` · 📤 <#${r.channelId}> [jump](https://discord.com/channels/${guildId}/${r.channelId}/${r.messageId}) · edited <t:${sec}:R>`
      : `\`${r.name}\` · 📝 draft · edited <t:${sec}:R>`;
  });
  const pages = [];
  for (let i = 0; i < lines.length; i += 15) {
    pages.push(guildEmbed(settings, `🧱 Saved embeds (${rows.length}/${GUILD_LIMITS.SAVED_EMBEDS})`, lines.slice(i, i + 15).join('\n')));
  }
  await paginate(interaction, pages);
}

async function deleteEmbed(interaction, client) {
  const name = normalizeEmbedName(interaction.options.getString('name', true));
  if (!name) return sendError(interaction, E_NAME());
  const row = await precheck(interaction, getSavedEmbed(interaction.guildId, name));
  if (row === undefined) return undefined;
  if (!row) return sendError(interaction, E_NOTFOUND(name));
  // A published embed lives in a channel: removing its record (and maybe the message) needs access to that channel.
  const where = row.channelId ? interaction.guild.channels.cache.get(row.channelId) : null;
  const noAccess = memberChannelProblem(where, interaction.member, ['ViewChannel']);
  if (noAccess) return sendError(interaction, errorEmbed('No Access', `**${name}** is published in <#${row.channelId}>. ${noAccess}`));
  const deleteMessage = interaction.options.getBoolean('delete_message') ?? false;
  const consequence = row.messageId
    ? (deleteMessage ? ' The published message will be deleted too.' : ' The published message stays but will no longer be tracked.')
    : '';
  const ok = await awaitConfirm(interaction, {
    title: 'Delete embed?',
    description: `Delete **${name}**?${consequence}`,
    confirmLabel: 'Delete',
    ephemeral: true,
  });
  if (!ok) return undefined; // awaitConfirm already showed the outcome
  const result = await deleteSavedEmbedAndMessage({ guild: interaction.guild, row, deleteMessage, client });
  if (!result.deleted) {
    await interaction.followUp({ embeds: [E_NOTFOUND(name)], ephemeral: true });
    return undefined;
  }
  const messageNote = result.messageDeleted
    ? ' The published message was deleted too.'
    : (deleteMessage && row.messageId ? ' (I could not delete the published message.)' : '');
  await interaction.followUp({ embeds: [successEmbed('Embed Deleted', `Deleted **${name}**.${messageNote}`)] });
  return undefined;
}

async function exportEmbed(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const row = await requireSavedEmbed(interaction.guildId, name);
  const json = templateToJson(normalizeTemplate(row.template));
  await interaction.editReply({
    embeds: [successEmbed('Embed Exported', `Here is **${name}** as JSON. Re-import it with \`/embed import\`.`)],
    files: [new AttachmentBuilder(Buffer.from(json, 'utf8'), { name: `embed-${name}.json` })],
  });
}

async function importEmbed(interaction) {
  await interaction.deferReply();
  const name = await nameOrReply(interaction);
  if (!name) return;
  const o = interaction.options;
  const attachment = o.getAttachment('file');
  const pasted = o.getString('json');
  if (!attachment && !pasted) {
    return sendError(interaction, errorEmbed('Nothing To Import', 'Attach a .json file or paste JSON into the json option.'));
  }
  if (attachment && pasted) return sendError(interaction, errorEmbed('Pick One', 'Use either file or json, not both.'));
  let text = pasted;
  if (attachment) {
    if (attachment.size > IMPORT_MAX_BYTES) return sendError(interaction, errorEmbed('File Too Large', 'The file must be 20 KB or smaller.'));
    try {
      text = await safeFetchText(attachment.url, { timeoutMs: 8000, maxBytes: IMPORT_MAX_BYTES });
    } catch (err) {
      if (!(err instanceof SafeFetchError)) throw err;
      logger.warn(`embeds: import download failed (${err.code})`);
      return sendError(interaction, errorEmbed('Download Failed', 'I could not download that file. Please try again.'));
    }
  }
  const { template, errors, warnings } = parseImportPayload(text);
  if (!template) return sendError(interaction, errorEmbed('Invalid JSON', errors.slice(0, 10).map((e) => `• ${e}`).join('\n')));

  const guildId = interaction.guildId;
  const existing = await getSavedEmbed(guildId, name);
  let saved;
  let title;
  if (existing) {
    if (!o.getBoolean('overwrite')) return sendError(interaction, E_TAKEN(name, ' Add overwrite:true to replace it.'));
    saved = await replaceTemplate(guildId, existing.id, template);
    title = 'Embed Replaced';
  } else {
    saved = await createSavedEmbed(guildId, name, template, interaction.user.id);
    title = 'Embed Imported';
  }
  const extra = warnings.map((w) => `\n⚠️ ${w}`).join('');
  await replyChanged(interaction, saved, title, `${title === 'Embed Imported' ? 'Imported' : 'Replaced'} **${name}**.${extra}`);
}

async function welcomeChannel(interaction) {
  await interaction.deferReply();
  const o = interaction.options;
  const settings = await getSettingsSafe(interaction.guildId);
  const r = await setUpWelcomeChannel({ interaction, create: o.getBoolean('create') ?? true, channel: o.getChannel('channel'), settings });
  if (!r.ok) {
    switch (r.code) {
      case 'LIMIT':
        return sendError(interaction, errorEmbed('Limit Reached', `The Welcome Channel needs ${r.needed} free embed slots; this server has ${r.count}/${GUILD_LIMITS.SAVED_EMBEDS}.`));
      case 'CHANNEL':
        return sendError(interaction, errorEmbed(
          'Missing Permissions',
          r.missing?.length ? `I need **${permissionNames(r.missing).join(', ')}** in ${r.channel} to post there.` : (r.reason ?? `I can't post in ${r.channel}.`),
        ));
      case 'ALREADY':
        return sendError(interaction, warnEmbed(
          'Already Set Up',
          `The Welcome Channel is already in <#${r.channelId}>. Edit the presets (welcome-greeting, welcome-rules, welcome-links, welcome-team) and run \`/embed update\`, or run this again with channel: to post them somewhere else.`,
        ));
      case 'BOT_PERMS':
        return sendError(interaction, errorEmbed('Missing Permissions', `I need **${permissionNames(r.missing).join(', ')}** to create the welcome channel.`));
      case 'TOO_MANY_CHANNELS':
        return sendError(interaction, errorEmbed('Too Many Channels', "This server has reached Discord's channel limit."));
      case 'WRONG_CHANNEL':
        return sendError(interaction, errorEmbed('Wrong Channel', 'Run this in a text channel, or pick one with channel:.'));
      case 'PARTIAL':
        return sendError(interaction, errorEmbed(
          'Partly Done',
          `I posted ${r.posted.length ? r.posted.map((n) => `\`${n}\``).join(', ') : 'nothing'} in ${r.channel}, then Discord refused the next one`
          + `${r.error?.message ? ` (${clampText(r.error.message, 200)})` : ''}. Not posted: ${r.unposted.map((n) => `\`${n}\``).join(', ')}. Fix the problem and run this again to post them.`,
        ));
      default:
        return sendError(interaction, E_DISCORD(r.error));
    }
  }
  const created = r.createdPresets.length ? `Created: ${r.createdPresets.map((n) => `\`${n}\``).join(', ')}.` : '';
  const reused = r.reusedPresets.length ? `Reused (not overwritten): ${r.reusedPresets.map((n) => `\`${n}\``).join(', ')}.` : '';
  let invite = '';
  if (r.invite?.source === 'vanity') invite = '\nThe links embed uses your vanity invite.';
  else if (r.invite?.source === 'created') invite = '\nI created a permanent invite for the links embed.';
  else if (r.invite) invite = '\nI could not add an invite button (no vanity URL, and I lack **Create Invite**); add one with `/embed button add name:welcome-links`.';
  const description = `Posted the greeting, rules, links and staff embeds in ${r.channel}.${r.createdChannel ? ' I created the channel; members can read but not write there.' : ''}`
    + '\nEdit them like any saved embed (for example `/embed edit name:welcome-rules` or `/embed button add name:welcome-links`), then run `/embed update` to refresh the posts.'
    + `${created || reused ? `\n${[created, reused].filter(Boolean).join(' ')}` : ''}${invite}${r.tip ? `\n${r.tip}` : ''}`;
  await interaction.editReply({ embeds: [successEmbed('Welcome Channel Ready', description)] });
}

const HANDLERS = Object.freeze({
  create: createEmbed,
  edit: editEmbed,
  'field add': fieldAdd,
  'field edit': fieldEdit,
  'field remove': fieldRemove,
  'field list': fieldList,
  'button add': buttonAdd,
  'button remove': buttonRemove,
  image: setImage,
  author: setAuthorLine,
  footer: setFooterLine,
  color: setSideColor,
  'title-url': setTitleLink,
  preview: previewEmbed,
  publish: publishEmbed,
  update: updateEmbed,
  list: listEmbeds,
  delete: deleteEmbed,
  export: exportEmbed,
  import: importEmbed,
  'welcome-channel': welcomeChannel,
});

export async function execute(interaction, client) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  const handler = HANDLERS[key];
  if (!handler) return;
  try {
    await handler(interaction, client);
  } catch (err) {
    if (isInteractionGone(err)) {
      logger.debug(`embeds: ${key}: interaction gone (${err?.code})`);
      return;
    }
    if (err instanceof EmbedEditError) {
      await sendError(interaction, embedForError(err));
      return;
    }
    logger.error(`embeds: ${key} failed`, err);
    await sendError(interaction, E_DB());
  }
}

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    const guildId = interaction.guildId;
    if (focused.name === 'name') {
      const rows = await searchSavedEmbeds(guildId, String(focused.value), 25);
      return await respondAutocomplete(interaction, rows.map((r) => ({
        name: r.messageId ? `${r.name} · published` : r.name,
        value: r.name,
      })));
    }
    if (focused.name === 'index') {
      const n = normalizeEmbedName(interaction.options.getString('name') ?? '');
      if (!n) return await respondAutocomplete(interaction, []);
      const row = await getSavedEmbed(guildId, n).catch(() => null);
      const tpl = normalizeTemplate(row?.template);
      const group = interaction.options.getSubcommandGroup(false); // 'field' | 'button'
      const items = group === 'button'
        ? tpl.buttons.map((b, i) => `${i + 1}. ${b.emoji ? `${b.emoji} ` : ''}${b.label} — ${b.url}`)
        : (tpl.embed?.fields ?? []).map((f, i) => `${i + 1}. ${f.name}`);
      const q = String(focused.value).trim().toLowerCase();
      const choices = items.map((label, i) => ({ name: label, value: i + 1 }))
        .filter((c) => !q || c.name.toLowerCase().includes(q))
        .slice(0, 25);
      return await respondAutocomplete(interaction, choices);
    }
    return await respondAutocomplete(interaction, []);
  } catch (err) {
    logger.debug(`embeds: autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}
