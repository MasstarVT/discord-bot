// /automation (pkg-automations.md §4): Automations as When -> If -> Do rules (MEE6 Automations), from presets or built step by step,
// with JSON export / import. Thin: data access and logic live in services/automationService.js.
import {
  SlashCommandBuilder, PermissionFlagsBits, ChannelType, AttachmentBuilder,
} from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { getSettingsSafe, isModuleEnabled } from '../../services/settingsService.js';
import { isRewardAvailable } from '../../services/rewards.js';
import * as auto from '../../services/automationService.js';
import {
  respondAutocomplete, awaitConfirm, checkChannelUsable, checkAssignableRole, permissionNames, safeDefer, isInteractionGone,
} from '../../utils/interactionHelpers.js';
import {
  successEmbed, errorEmbed, warnEmbed, guildEmbed,
} from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import { safeFetchText, SafeFetchError } from '../../utils/safeFetch.js';
import { clampText, chunkLines, formatNumber } from '../../utils/text.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';

export const module = 'automations';
export const gated = false;                          // config works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;
// no subcommandPermissions, no allowInDMs -> guild-only (buildCommandPayload adds contexts [0])

// ── Builder (pkg-automations.md §4.2, verbatim) ──────────────────────────────

const TRIGGER_CHOICES = [
  { name: 'Message sent', value: 'MESSAGE_SENT' },
  { name: 'Message deleted', value: 'MESSAGE_DELETED' },
  { name: 'Message edited', value: 'MESSAGE_EDITED' },
  { name: 'Button clicked', value: 'BUTTON_CLICKED' },
  { name: 'Reaction added', value: 'REACTION_ADDED' },
  { name: 'Role gained', value: 'ROLE_GAINED' },
  { name: 'Role lost', value: 'ROLE_LOST' },
  { name: 'Joined voice', value: 'VOICE_JOINED' },
  { name: 'Left voice', value: 'VOICE_LEFT' },
  { name: 'Thread created', value: 'THREAD_CREATED' },
  { name: 'Member joined', value: 'MEMBER_JOINED' },
];
const PRESET_CHOICES = [
  { name: 'Welcomer', value: 'welcomer' },
  { name: 'Starboard', value: 'starboard' },
  { name: 'Thread Joiner', value: 'thread-joiner' },
];
const PRESET_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildMedia];
const TARGET_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice,
  ChannelType.GuildStageVoice, ChannelType.PublicThread, ChannelType.PrivateThread, ChannelType.AnnouncementThread];
const nameOpt = (o) => o.setName('name').setDescription('Automation').setRequired(true).setAutocomplete(true).setMaxLength(32);
const newNameOpt = (o) => o.setName('name').setDescription('Name (a-z, 0-9, - and _)').setRequired(true).setMaxLength(32);
const cooldownOpt = (o) => o.setName('cooldown').setDescription('Cooldown per user in seconds').setMinValue(0).setMaxValue(604800);
const indexOpt = (what, max) => (o) => o.setName('index').setDescription(`${what} number`).setRequired(true)
  .setMinValue(1).setMaxValue(max).setAutocomplete(true);

export const data = new SlashCommandBuilder()
  .setName('automation')
  .setDescription('Automations: When → If → Do rules (MEE6 Automations)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('create').setDescription('Create from a preset')
    .addStringOption((o) => o.setName('preset').setDescription('Preset recipe').setRequired(true).addChoices(...PRESET_CHOICES))
    .addStringOption(newNameOpt)
    .addChannelOption((o) => o.setName('channel').setDescription('Post/repost here (Thread Joiner: watch here)').addChannelTypes(...PRESET_CHANNEL_TYPES))
    .addStringOption((o) => newChannelOption(o))
    .addStringOption((o) => o.setName('message').setDescription('Welcomer: message').setMaxLength(2000))
    .addRoleOption((o) => o.setName('role').setDescription('Welcomer: role to give'))
    .addStringOption((o) => newRoleOption(o))
    .addStringOption((o) => o.setName('emoji').setDescription('Starboard: emoji (default ⭐)').setMaxLength(64))
    .addIntegerOption((o) => o.setName('threshold').setDescription('Starboard: threshold (default 3)').setMinValue(1).setMaxValue(1000))
    .addUserOption((o) => o.setName('user').setDescription('Thread Joiner: member to add')))
  .addSubcommand((s) => s.setName('create-blank').setDescription('Create an empty automation')
    .addStringOption(newNameOpt)
    .addStringOption((o) => o.setName('trigger').setDescription('When…').setRequired(true).addChoices(...TRIGGER_CHOICES))
    .addIntegerOption(cooldownOpt))
  .addSubcommand((s) => s.setName('edit').setDescription('Rename or change the cooldown')
    .addStringOption(nameOpt)
    .addStringOption((o) => o.setName('rename').setDescription('New name').setMaxLength(32))
    .addIntegerOption(cooldownOpt))
  .addSubcommandGroup((g) => g.setName('condition').setDescription('If: conditions (all must pass)')
    .addSubcommand((s) => s.setName('add').setDescription('Add a condition')
      .addStringOption(nameOpt)
      .addStringOption((o) => o.setName('type').setDescription('Condition type').setRequired(true).setAutocomplete(true).setMaxLength(32))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category'))
      .addRoleOption((o) => o.setName('role').setDescription('Role'))
      .addUserOption((o) => o.setName('user').setDescription('User'))
      .addStringOption((o) => o.setName('text').setDescription('Words (comma-separated), regex, emoji or mentions').setMaxLength(1000))
      .addIntegerOption((o) => o.setName('number').setDescription('Minimum reaction count').setMinValue(1).setMaxValue(1000))
      .addStringOption((o) => o.setName('button').setDescription('Automation button').setAutocomplete(true).setMaxLength(64)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a condition')
      .addStringOption(nameOpt)
      .addIntegerOption(indexOpt('Condition', LIMITS.AUTOMATION_CONDITIONS))))
  .addSubcommandGroup((g) => g.setName('action').setDescription('Do: actions (run in order)')
    .addSubcommand((s) => s.setName('add').setDescription('Add an action')
      .addStringOption(nameOpt)
      .addStringOption((o) => o.setName('type').setDescription('Action type').setRequired(true).setAutocomplete(true).setMaxLength(32))
      .addChannelOption((o) => o.setName('channel').setDescription('Target channel (default: where it happened)').addChannelTypes(...TARGET_CHANNEL_TYPES))
      .addStringOption((o) => newChannelOption(o))
      .addStringOption((o) => o.setName('text').setDescription('Message, thread name, or role/user mentions').setMaxLength(2000))
      .addRoleOption((o) => o.setName('role').setDescription('Role to give or take'))
      .addStringOption((o) => newRoleOption(o))
      .addUserOption((o) => o.setName('user').setDescription('Member to add to the thread'))
      .addStringOption((o) => o.setName('emoji').setDescription('Emoji').setMaxLength(64))
      .addIntegerOption((o) => o.setName('amount').setDescription('Coins or XP').setMinValue(1).setMaxValue(1000000))
      .addStringOption((o) => o.setName('embed').setDescription('Saved embed').setAutocomplete(true).setMaxLength(32))
      .addStringOption((o) => o.setName('button_label').setDescription('Button label').setMaxLength(80))
      .addStringOption((o) => o.setName('button_url').setDescription('Button link (empty = automation button)').setMaxLength(512)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove an action')
      .addStringOption(nameOpt)
      .addIntegerOption(indexOpt('Action', LIMITS.AUTOMATION_ACTIONS))))
  .addSubcommand((s) => s.setName('list').setDescription('List automations'))
  .addSubcommand((s) => s.setName('info').setDescription('Show an automation').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('enable').setDescription('Turn an automation on').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn an automation off').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete an automation').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('export').setDescription('Download as JSON').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('import').setDescription('Create or replace from JSON')
    .addAttachmentOption((o) => o.setName('file').setDescription('JSON file (max 32 KB)').setRequired(true))
    .addStringOption((o) => o.setName('name').setDescription('Name (default: from the file)').setMaxLength(32))
    .addBooleanOption((o) => o.setName('overwrite').setDescription('Replace if the name exists')));

// ── Replies ──────────────────────────────────────────────────────────────────

const {
  AutomationError, TRIGGERS, CONDITION_TYPES, ACTION_TYPES, ERROR_TEXTS, IMPORT_MAX_BYTES,
  normalizeAutomationName, normalizeTypeInput, describeCondition, describeAction,
} = auto;

const MODULE_OFF_LINE = '\n\n⚠️ The Automations module is off, so nothing runs. Turn it on with `/module enable automations`.';
const offLine = (name) => `\nThis automation is **off**. Turn it on with \`/automation enable name:${name}\`.`;
const ignoredLine = (ignored) => (ignored?.length ? `\nIgnored for this type: ${ignored.map((n) => `\`${n}\``).join(', ')}.` : '');

const E = {
  name: () => errorEmbed('Invalid Name', ERROR_TEXTS.name),
  notFound: (name) => errorEmbed('Automation Not Found', ERROR_TEXTS.notFound(name)),
  taken: (name, extra = '') => errorEmbed('Name Taken', `${ERROR_TEXTS.taken(name)}${extra}`),
  limit: () => errorEmbed('Limit Reached', ERROR_TEXTS.limit()),
  nothing: () => errorEmbed('Nothing To Change', 'Give at least one of the optional values.'),
  perms: (missing, channel, reason) => errorEmbed('Missing Permissions', missing?.length
    ? `I need **${permissionNames(missing).join(', ')}** in ${channel ? `<#${channel.id}>` : 'that channel'} for that.`
    : (reason ?? 'I cannot use that channel.')),
  reward: (kind) => errorEmbed('Not Available', `${kind === 'xp' ? 'XP' : 'Coin'} rewards need the ${kind === 'xp' ? 'Levels' : 'Economy'} module. Turn it on with \`/module enable ${kind === 'xp' ? 'leveling' : 'economy'}\`.`),
  noAccess: (text) => errorEmbed('No Access', text),
  db: () => errorEmbed('Something Went Wrong', 'I could not do that right now. Please try again in a moment.'),
};

/** editReply that never throws for an interaction that is gone. */
async function send(interaction, embed, extra = {}) {
  try {
    return await interaction.editReply({ embeds: [embed], ...extra });
  } catch (err) {
    if (isInteractionGone(err)) { logger.debug(`automations: reply dropped (${err?.code})`); return null; }
    throw err;
  }
}

/** Maps an error thrown by the service to the embed the admin sees. */
function embedForError(err, ctx) {
  if (auto.isUniqueViolation(err)) return E.taken(ctx.name ?? '?', ctx.takenHint ?? '');
  if (err instanceof AutomationError) {
    switch (err.code) {
      case 'NOTFOUND': return errorEmbed('Automation Not Found', err.message);
      case 'LIMIT': return E.limit();
      case 'NAME': return E.name();
      case 'NOACTIONS': return errorEmbed('No Actions', err.message);
      case 'NOSUCH': return errorEmbed(ctx.removeKind === 'condition' ? 'No Such Condition' : 'No Such Action', err.message);
      case 'HIERARCHY': return errorEmbed('Role Not Allowed', err.message);
      default: return errorEmbed('Could Not Save', err.message);
    }
  }
  return null;
}

/** The embed for a failed option mapping (conditionParamsFromOptions / actionParamsFromOptions). */
function embedForParams(p) {
  switch (p.errorKind) {
    case 'perms': return E.perms(p.missing, p.channel, p.error);
    case 'role': return errorEmbed('Role Not Allowed', p.error);
    case 'ping': return errorEmbed('Role Not Pingable', p.error);
    case 'reward': return E.reward(p.rewardKind);
    case 'embed': return errorEmbed('Embed Not Found', `There is no saved embed called **${p.embedName}**. See \`/embed list\`.`);
    default: return errorEmbed('Invalid Value', p.error);
  }
}

const summaryOf = (row, { guild, settings }) => {
  const conds = row.conditions.map((c) => describeCondition(c, { guild })).join(' · ') || 'always';
  const acts = row.actions.map((a) => describeAction(a, { guild, settings })).join(' · ') || 'nothing yet';
  return clampText(`**${row.name}** — ${TRIGGERS[row.trigger].label}\n**If:** ${conds}\n**Do:** ${acts}`, 1500);
};

/** The shared tail of a success description. */
const tail = (ctx, { disabledName = null, ignored = [] } = {}) => `${disabledName ? offLine(disabledName) : ''}${ignoredLine(ignored)}${ctx.moduleOff ? MODULE_OFF_LINE : ''}`;

/**
 * An automation that gives or takes a role at or above the admin's highest role cannot be changed, enabled or disabled by that
 * admin (the guild owner is exempt). Runs on the locked row inside every mutation, so the check sees what is really stored.
 */
function guardRoles(ctx, cur) {
  if (!cur.actions.some((a) => a.roleIds?.length)) return;
  const problem = auto.importRoleHierarchyProblem(ctx.guild, ctx.interaction.member, cur.actions);
  if (problem) throw new AutomationError(`${problem}\nThis automation gives or takes a role at or above your highest role, so you cannot change it.`, 'HIERARCHY');
}

function nameFrom(ctx, optionName = 'name') {
  const name = normalizeAutomationName(ctx.interaction.options.getString(optionName, true));
  if (name) ctx.name = name;
  return name;
}

async function loadRow(ctx, name) {
  const row = await auto.getAutomationByName(ctx.guildId, name);
  if (!row) await send(ctx.interaction, E.notFound(name));
  return row;
}

// ── Subcommands ──────────────────────────────────────────────────────────────

const optionNamesGiven = (o, names) => names.filter((n) => o.get(n) !== null && o.get(n) !== undefined);

async function create(ctx) {
  const { interaction, guild, guildId, settings } = ctx;
  const o = interaction.options;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const preset = o.getString('preset', true);
  const channel = o.getChannel('channel');
  const role = o.getRole('role');
  const user = o.getUser('user');
  const message = o.getString('message');
  const emojiRaw = o.getString('emoji');
  const threshold = o.getInteger('threshold');
  const given = optionNamesGiven(o, ['channel', 'message', 'role', 'emoji', 'threshold', 'user']);
  const ignored = given.filter((n) => !auto.PRESET_OPTIONS[preset].includes(n));
  const opts = {};
  const fullChannel = channel ? (guild.channels.cache.get(channel.id) ?? channel) : null;
  const textLike = fullChannel && (fullChannel.type === ChannelType.GuildText || fullChannel.type === ChannelType.GuildAnnouncement);

  if (preset === 'welcomer') {
    if (!channel) return send(interaction, errorEmbed('Channel Needed', 'The Welcomer preset needs a channel to post in.'));
    if (!textLike) return send(interaction, errorEmbed('Wrong Channel Type', 'Pick a text or announcement channel.'));
    const check = checkChannelUsable(fullChannel, { needs: ['ViewChannel', 'SendMessages'] });
    if (!check.ok) return send(interaction, E.perms(check.missing, fullChannel, check.reason));
    if (role) {
      const rc = checkAssignableRole(guild, guild.roles.cache.get(role.id) ?? role, { executor: interaction.member });
      if (!rc.ok) return send(interaction, errorEmbed('Role Not Allowed', rc.reason));
    }
    const ping = auto.rolePingRefusal(interaction.member, fullChannel, message);
    if (ping) return send(interaction, errorEmbed('Role Not Pingable', ping));
    Object.assign(opts, { channelId: channel.id, message: message?.trim() || null, roleId: role?.id ?? null });
  } else if (preset === 'starboard') {
    if (!channel) return send(interaction, errorEmbed('Channel Needed', 'The Starboard preset needs a channel to repost to.'));
    if (!textLike) return send(interaction, errorEmbed('Wrong Channel Type', 'Pick a text or announcement channel.'));
    const check = checkChannelUsable(fullChannel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
    if (!check.ok) return send(interaction, E.perms(check.missing, fullChannel, check.reason));
    let emoji = null;
    if (emojiRaw?.trim()) {
      const parsed = auto.parseEmojiInput(emojiRaw, auto.emojiResolver(guild));
      if (!parsed.ok) return send(interaction, errorEmbed('Invalid Emoji', parsed.error));
      emoji = parsed.value;
    }
    Object.assign(opts, { channelId: channel.id, emoji, threshold });
  } else if (preset === 'thread-joiner') {
    if (!user) return send(interaction, errorEmbed('Member Needed', 'The Thread Joiner preset needs a member to add to new threads.'));
    if (!o.getMember('user')) return send(interaction, errorEmbed('Not A Member', 'That user is not in this server.'));
    if (user.bot) return send(interaction, errorEmbed('Not A Member', 'Pick a person, not a bot.'));
    Object.assign(opts, { userId: user.id, channelId: channel?.id ?? null });
  } else {
    return send(interaction, errorEmbed('Unknown Preset', 'Pick a preset from the list.'));
  }

  const built = auto.buildPresetAutomation(preset, opts);
  const access = auto.actionChannelAccessProblem(guild, interaction.member, built.actions);
  if (access) return send(interaction, E.noAccess(access));
  const row = await auto.createAutomation(guildId, { name, ...built, enabled: true, cooldownSeconds: 0 }, interaction.user.id);
  return send(interaction, successEmbed('Automation Created', `${summaryOf(row, { guild, settings })}${tail(ctx, { ignored })}`));
}

async function createBlank(ctx) {
  const { interaction, guildId } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const trigger = interaction.options.getString('trigger', true);
  const cooldown = interaction.options.getInteger('cooldown') ?? 0;
  await auto.createAutomation(guildId, { name, trigger, conditions: [], actions: [], enabled: false, cooldownSeconds: cooldown }, interaction.user.id);
  return send(interaction, successEmbed('Automation Created', `**${name}** (${TRIGGERS[trigger].label}) was created **off**.\nAdd conditions with \`/automation condition add\` and actions with \`/automation action add\`, then turn it on with \`/automation enable\`.${ctx.moduleOff ? MODULE_OFF_LINE : ''}`));
}

async function edit(ctx) {
  const { interaction, guildId } = ctx;
  const o = interaction.options;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const renameRaw = o.getString('rename');
  const cooldown = o.getInteger('cooldown');
  if (renameRaw === null && cooldown === null) return send(interaction, E.nothing());
  let newName = null;
  if (renameRaw !== null) {
    newName = normalizeAutomationName(renameRaw);
    if (!newName) return send(interaction, E.name());
    ctx.name = newName;   // a P2002 on the rename reports the new name
  }
  const { row, result } = await auto.mutateAutomation(guildId, name, (cur) => {
    guardRoles(ctx, cur);
    const patch = {};
    if (newName && newName !== cur.name) patch.name = newName;
    if (cooldown !== null && cooldown !== cur.cooldownSeconds) patch.cooldownSeconds = cooldown;
    return { data: patch, result: { oldName: cur.name, oldCooldown: cur.cooldownSeconds, changed: Object.keys(patch) } };
  });
  if (!result.changed.length) return send(interaction, warnEmbed('No Change', `**${row.name}** already has those values.`));
  const lines = [];
  if (result.changed.includes('name')) lines.push(`• **Name:** ${result.oldName} → ${row.name}`);
  if (result.changed.includes('cooldownSeconds')) lines.push(`• **Cooldown:** ${result.oldCooldown} s → ${row.cooldownSeconds} s`);
  return send(interaction, successEmbed('Automation Updated', `${lines.join('\n')}${tail(ctx, { disabledName: row.enabled ? null : row.name })}`));
}

async function conditionAdd(ctx) {
  const { interaction, guild, guildId, settings } = ctx;
  const o = interaction.options;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const row = await loadRow(ctx, name);
  if (!row) return undefined;
  const type = normalizeTypeInput(o.getString('type', true));
  if (!Object.hasOwn(CONDITION_TYPES, type)) return send(interaction, errorEmbed('Unknown Type', 'Unknown condition type. Pick one from the list.'));
  const def = CONDITION_TYPES[type];
  if (!def.triggers.has(row.trigger)) {
    return send(interaction, errorEmbed('Not Available', `"${def.label}" doesn't work with the trigger "${TRIGGERS[row.trigger].label}".`));
  }
  const opts = {
    channel: o.getChannel('channel'), role: o.getRole('role'), user: o.getUser('user'),
    text: o.getString('text'), number: o.getInteger('number'), button: o.getString('button'),
  };
  const p = await auto.conditionParamsFromOptions(type, opts, { guild, member: interaction.member, settings, row });
  if (!p.ok) return send(interaction, embedForParams(p));
  const { row: saved, result } = await auto.mutateAutomation(guildId, name, (cur) => {
    guardRoles(ctx, cur);
    const r = auto.addCondition(cur, type, p.params);
    return { data: r.addedNew ? { conditions: r.conditions } : {}, result: r };
  });
  if (result.merged && !result.addedNew) return send(interaction, warnEmbed('Already There', 'Those values are already in the condition.'));
  const cond = result.conditions[result.index - 1];
  const text = result.merged
    ? `Condition ${result.index} of **${name}** is now: ${describeCondition(cond, { guild })}`
    : `**${name}** now also checks: ${describeCondition(cond, { guild })}`;
  return send(interaction, successEmbed(result.merged ? 'Condition Updated' : 'Condition Added',
    `${text}${tail(ctx, { disabledName: saved.enabled ? null : name, ignored: p.ignored })}`));
}

async function conditionRemove(ctx) {
  const { interaction, guild, guildId } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const index = interaction.options.getInteger('index', true);
  ctx.removeKind = 'condition';
  const { row, result } = await auto.mutateAutomation(guildId, name, (cur) => {
    guardRoles(ctx, cur);
    if (index < 1 || index > cur.conditions.length) {
      throw new AutomationError(`**${name}** has ${cur.conditions.length} condition(s).`, 'NOSUCH');
    }
    const r = auto.removeAt(cur.conditions, index);
    return { data: { conditions: r.list }, result: r };
  });
  return send(interaction, successEmbed('Condition Removed',
    `Removed: ${describeCondition(result.removed, { guild })}${tail(ctx, { disabledName: row.enabled ? null : name })}`));
}

async function actionAdd(ctx) {
  const { interaction, guild, guildId, settings } = ctx;
  const o = interaction.options;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const row = await loadRow(ctx, name);
  if (!row) return undefined;
  const type = normalizeTypeInput(o.getString('type', true));
  if (!Object.hasOwn(ACTION_TYPES, type)) return send(interaction, errorEmbed('Unknown Type', 'Unknown action type. Pick one from the list.'));
  const def = ACTION_TYPES[type];
  if (!def.triggers.has(row.trigger)) {
    return send(interaction, errorEmbed('Not Available', `"${def.label}" doesn't work with the trigger "${TRIGGERS[row.trigger].label}".`));
  }
  const opts = {
    channel: o.getChannel('channel'), text: o.getString('text'), role: o.getRole('role'), user: o.getUser('user'),
    emoji: o.getString('emoji'), amount: o.getInteger('amount'), embed: o.getString('embed'),
    buttonLabel: o.getString('button_label'), buttonUrl: o.getString('button_url'),
  };
  const p = await auto.actionParamsFromOptions(type, opts, { guild, member: interaction.member, settings, row, channel: interaction.channel });
  if (!p.ok) return send(interaction, embedForParams(p));
  const access = auto.actionChannelAccessProblem(guild, interaction.member, [{ type, ...p.params }], { first: row.actions.length + 1 });
  if (access) return send(interaction, E.noAccess(access));
  const { row: saved, result } = await auto.mutateAutomation(guildId, name, (cur) => {
    guardRoles(ctx, cur);
    const r = auto.appendAction({ ...cur, buttonSeq: cur.buttonSeq }, { type, ...p.params });
    return { data: { actions: r.actions, ...(r.buttonSeq != null ? { buttonSeq: r.buttonSeq } : {}) }, result: r };
  });
  const action = result.actions[result.index - 1];
  const buttonNote = result.buttonIndex != null
    ? `\nIts button number is **${result.buttonIndex}**: use it in a "Button clicked" automation with the condition "Clicked this button".`
    : '';
  return send(interaction, successEmbed('Action Added',
    `Action ${result.index} of **${name}**: ${describeAction(action, { guild, settings })}${buttonNote}${tail(ctx, { disabledName: saved.enabled ? null : name, ignored: p.ignored })}`));
}

async function actionRemove(ctx) {
  const { interaction, guild, guildId, settings } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const index = interaction.options.getInteger('index', true);
  ctx.removeKind = 'action';
  const { row, result } = await auto.mutateAutomation(guildId, name, (cur) => {
    guardRoles(ctx, cur);
    if (index < 1 || index > cur.actions.length) {
      throw new AutomationError(`**${name}** has ${cur.actions.length} action(s).`, 'NOSUCH');
    }
    const r = auto.removeAt(cur.actions, index);
    const turnedOff = r.list.length === 0 && cur.enabled;
    return { data: { actions: r.list, ...(turnedOff ? { enabled: false } : {}) }, result: { ...r, turnedOff } };
  });
  const note = result.turnedOff ? '\nIt had no actions left, so it was turned **off**.' : '';
  return send(interaction, successEmbed('Action Removed',
    `Removed: ${describeAction(result.removed, { guild, settings })}${note}${tail(ctx, { disabledName: row.enabled || result.turnedOff ? null : name })}`));
}

async function list(ctx) {
  const { interaction, settings } = ctx;
  const rows = await auto.listAutomations(ctx.guildId);
  if (!rows.length) {
    return send(interaction, guildEmbed(settings, '⚙️ Automations', `No automations yet. Start with \`/automation create\` (presets) or \`/automation create-blank\`.${ctx.moduleOff ? MODULE_OFF_LINE : ''}`));
  }
  const lines = rows.map((r) => `${r.enabled ? '🟢' : '⚫'} \`${r.name}\` · ${TRIGGERS[r.trigger].label} · ${r.conditions.length} if · ${r.actions.length} do · ${formatNumber(r.uses)} runs`);
  const pages = [];
  for (let i = 0; i < lines.length; i += 15) {
    pages.push(guildEmbed(settings, `⚙️ Automations (${rows.length}/${LIMITS.AUTOMATIONS})`,
      `${lines.slice(i, i + 15).join('\n')}${ctx.moduleOff ? MODULE_OFF_LINE : ''}`));
  }
  await paginate(interaction, pages);
  return undefined;
}

function fieldsFor(title, lines) {
  return chunkLines(lines, 1024).map((value, k) => ({ name: k === 0 ? title : `${title.replace(/ \(\d+\)$/, '')} (cont.)`, value }));
}

async function info(ctx) {
  const { interaction, guild, settings } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const row = await loadRow(ctx, name);
  if (!row) return undefined;
  const cd = row.cooldownSeconds ? `${row.cooldownSeconds} s` : 'none';
  const header = `${row.enabled ? '🟢 On' : '⚫ Off'} · **When:** ${TRIGGERS[row.trigger].label} · Cooldown: ${cd} · Runs: ${formatNumber(row.uses)}${ctx.moduleOff ? MODULE_OFF_LINE : ''}`;
  const embed = guildEmbed(settings, `⚙️ ${row.name}`, header);
  const conds = row.conditions.map((c, i) => clampText(`${i + 1}. ${describeCondition(c, { guild })}`, 150));
  const acts = row.actions.map((a, i) => clampText(`${i + 1}. ${describeAction(a, { guild, settings })}`, 150));
  embed.addFields(...fieldsFor(`If (${row.conditions.length})`, conds.length ? conds : ['Always']));
  embed.addFields(...fieldsFor(`Do (${row.actions.length})`, acts.length ? acts : ['No actions yet — add one with /automation action add.']));
  const problems = await auto.automationProblems(guild, row, settings);
  if (problems.length) {
    embed.addFields({ name: 'Problems', value: clampText(problems.map((p) => clampText(`⚠️ ${p}`, 200)).join('\n'), 1024) });
  }
  return send(interaction, embed);
}

async function setEnabled(ctx, enable) {
  const { interaction, guild, settings } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const { row, result } = await auto.mutateAutomation(ctx.guildId, name, (cur) => {
    guardRoles(ctx, cur);
    if (enable && cur.actions.length === 0) throw new AutomationError(ERROR_TEXTS.needsActions, 'NOACTIONS');
    if (cur.enabled === enable) return { data: {}, result: { unchanged: true } };
    return { data: { enabled: enable }, result: { unchanged: false } };
  });
  if (result.unchanged) return send(interaction, warnEmbed('No Change', `**${name}** is already ${enable ? 'on' : 'off'}.`));
  let extra = '';
  if (enable) {
    const problems = await auto.automationProblems(guild, row, settings);
    if (problems.length) {
      extra = `${problems.slice(0, 5).map((p) => `\n⚠️ ${clampText(p, 200)}`).join('')}${problems.length > 5 ? `\n… and ${problems.length - 5} more (see /automation info)` : ''}`;
    }
  }
  return send(interaction, successEmbed(enable ? 'Automation Enabled' : 'Automation Disabled',
    `**${name}** is now ${enable ? 'on' : 'off'}.${extra}${ctx.moduleOff ? MODULE_OFF_LINE : ''}`));
}

async function del(ctx) {
  const { interaction, guildId } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const row = await auto.getAutomationByName(guildId, name);
  if (!row) return send(interaction, E.notFound(name));
  const confirmed = await awaitConfirm(interaction, {
    title: 'Delete automation?',
    description: `Delete **${name}**? Buttons it already posted stop doing anything unless another automation listens for them.`,
    confirmLabel: 'Delete',
  });
  if (!confirmed) return undefined;
  const count = await auto.deleteAutomation(guildId, row.id);
  if (!count) return interaction.followUp({ embeds: [E.notFound(name)], ephemeral: true });
  return interaction.followUp({ embeds: [successEmbed('Automation Deleted', `Deleted **${name}**.`)] });
}

async function exportOne(ctx) {
  const { interaction, guildId } = ctx;
  const name = nameFrom(ctx);
  if (!name) return send(interaction, E.name());
  const row = await loadRow(ctx, name);
  if (!row) return undefined;
  const refs = await auto.listAutomationRefs(guildId);
  const json = auto.toExportJson(row, new Map(refs.map((r) => [r.id, { name: r.name }])));
  return send(interaction, successEmbed('Automation Exported', `Here is **${name}** as JSON. Import it with \`/automation import\`.`), {
    files: [new AttachmentBuilder(Buffer.from(json, 'utf8'), { name: `automation-${name}.json` })],
  });
}

async function importJson(ctx) {
  const { interaction, guild, guildId, settings } = ctx;
  const o = interaction.options;
  const file = o.getAttachment('file', true);
  const overwrite = o.getBoolean('overwrite') ?? false;
  if (file.size > IMPORT_MAX_BYTES) return send(interaction, errorEmbed('File Too Large', 'The file must be 32 KB or smaller.'));
  const looksJson = /\.json$/i.test(file.name ?? '') || /json|text\/plain/i.test(file.contentType ?? '');
  if (!looksJson) return send(interaction, errorEmbed('Wrong File', 'Attach a .json file from /automation export.'));

  let text;
  try {
    text = await safeFetchText(file.url, { timeoutMs: 8000, maxBytes: IMPORT_MAX_BYTES });
  } catch (err) {
    if (err instanceof SafeFetchError) return send(interaction, errorEmbed('Download Failed', 'I could not download that file. Please try again.'));
    throw err;
  }
  const parsed = auto.parseAutomationJson(text);
  if (!parsed.ok) return send(interaction, errorEmbed('Invalid JSON', parsed.errors.slice(0, 10).map((e) => `• ${e}`).join('\n')));

  // Same hierarchy rule as `action add` / the Welcomer preset: an import must not hand out a role above the importer's own.
  const escalation = auto.importRoleHierarchyProblem(guild, interaction.member, parsed.data.actions);
  if (escalation) return send(interaction, errorEmbed('Role Not Allowed', escalation));
  // The importer must also be allowed to post where the file posts, and to ping the roles it pings.
  const access = auto.actionChannelAccessProblem(guild, interaction.member, parsed.data.actions);
  if (access) return send(interaction, E.noAccess(access));
  const pingProblem = auto.importRolePingProblem(guild, interaction.member, parsed.data.actions, { fallbackChannel: interaction.channel });
  if (pingProblem) return send(interaction, errorEmbed('Role Not Pingable', pingProblem));

  const name = normalizeAutomationName(o.getString('name') ?? parsed.data.name ?? '');
  if (!name) return send(interaction, E.name());
  ctx.name = name;
  const existing = await auto.getAutomationByName(guildId, name);
  if (existing && !overwrite) return send(interaction, E.taken(name, ' Add overwrite:True to replace it.'));
  // Replacing is a write to the existing automation: refused when it gives or takes a role the importer outranks.
  const replaced = existing && existing.actions.some((a) => a.roleIds?.length) ? auto.importRoleHierarchyProblem(guild, interaction.member, existing.actions) : null;
  if (replaced) return send(interaction, errorEmbed('Role Not Allowed', `${replaced}\nThe automation you would replace gives or takes a role at or above your highest role.`));

  const { data, warnings, info: notes } = await auto.resolveImportReferences(guild, parsed.data, { settings, existingRow: existing });
  const enabled = parsed.data.enabled && warnings.length === 0 && data.actions.length > 0;
  const payload = { name, trigger: data.trigger, conditions: data.conditions, actions: data.actions, enabled, cooldownSeconds: data.cooldownSeconds };

  let row;
  if (existing) row = await auto.replaceAutomation(guildId, existing.id, payload);
  else row = await auto.createAutomation(guildId, payload, interaction.user.id, { selfRefId: data.exportedId });

  let extra = '';
  if (warnings.length) {
    extra += `\n\n⚠️ It was saved **off** because:\n${warnings.slice(0, 8).map((w) => `• ${w}`).join('\n')}`;
    if (warnings.length > 8) extra += `\n… and ${warnings.length - 8} more`;
  }
  if (notes.length) extra += `\n\nℹ️ ${notes.slice(0, 5).join(' ')}`;
  return send(interaction, successEmbed(existing ? 'Automation Replaced' : 'Automation Imported',
    `${summaryOf(row, { guild, settings })}${extra}${ctx.moduleOff ? MODULE_OFF_LINE : ''}`));
}

const HANDLERS = Object.freeze({
  __proto__: null,
  create,
  'create-blank': createBlank,
  edit,
  'condition add': conditionAdd,
  'condition remove': conditionRemove,
  'action add': actionAdd,
  'action remove': actionRemove,
  list,
  info,
  enable: (ctx) => setEnabled(ctx, true),
  disable: (ctx) => setEnabled(ctx, false),
  delete: del,
  export: exportOne,
  import: importJson,
});

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  const handler = HANDLERS[key];
  if (!handler) return;
  const ctx = { interaction, guild: interaction.guild, guildId: interaction.guildId, settings: null, moduleOff: false, name: null };
  try {
    // Every subcommand answers publicly (admin convention) except delete, whose confirm dialog is private.
    if (!(await safeDefer(interaction, { ephemeral: key === 'delete' }))) return;
    if (!(await resolveNewOptions(interaction, data))) return;
    ctx.settings = await getSettingsSafe(ctx.guildId);   // a memo hit (the router just loaded it); null is tolerated
    ctx.moduleOff = !isModuleEnabled(ctx.settings, 'automations');
    await handler(ctx);
  } catch (err) {
    if (isInteractionGone(err)) { logger.debug(`automations: ${key}: interaction gone (${err?.code})`); return; }
    const embed = embedForError(err, ctx);
    if (embed) {
      await send(interaction, embed).catch((e) => logger.debug(`automations: error reply failed (${e?.message})`));
      return;
    }
    logger.error(`automations: ${key} failed`, err);
    await send(interaction, E.db()).catch((e) => logger.debug(`automations: error reply failed (${e?.message})`));
  }
}

// ── Autocomplete (spec 4.6) ──────────────────────────────────────────────────

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    const guildId = interaction.guildId;
    const q = String(focused.value ?? '');
    const group = interaction.options.getSubcommandGroup(false);

    if (focused.name === 'name') {
      const rows = await auto.searchAutomations(guildId, normalizeAutomationName(q) ?? '');
      return await respondAutocomplete(interaction, rows.map((r) => ({
        name: `${r.name} · ${TRIGGERS[r.trigger]?.label ?? r.trigger}${r.enabled ? '' : ' (off)'}`,
        value: r.name,
      })));
    }

    if (focused.name === 'embed') {
      const names = await auto.searchSavedEmbedNames(guildId, q);
      return await respondAutocomplete(interaction, names.map((n) => ({ name: n, value: n })));
    }

    if (focused.name === 'button') {
      const needle = q.trim().toLowerCase();
      const buttons = await auto.listAutomationButtons(guildId);
      return await respondAutocomplete(interaction, buttons
        .map((b) => ({ name: `${b.automationName} · button ${b.index} "${b.label}"`, value: `${b.automationId}:${b.index}` }))
        .filter((c) => !needle || c.name.toLowerCase().includes(needle)));
    }

    // type and index both depend on the automation named in the same command.
    const nameValue = normalizeAutomationName(interaction.options.getString('name') ?? '');
    const row = nameValue ? await auto.getAutomationByName(guildId, nameValue).catch(() => null) : null;

    if (focused.name === 'type') {
      const needle = normalizeTypeInput(q);
      const loose = q.trim().toLowerCase();
      const matches = (key, label) => !loose || key.includes(needle) || label.toLowerCase().includes(loose);
      let choices;
      if (group === 'condition') {
        choices = Object.entries(CONDITION_TYPES)
          .filter(([key, d]) => (!row || d.triggers.has(row.trigger)) && matches(key, d.label))
          .map(([key, d]) => ({ name: d.label, value: key }));
      } else {
        const settings = await getSettingsSafe(guildId);
        const hasThread = row?.actions.some((a) => a.type === 'CREATE_THREAD') ?? false;
        choices = Object.entries(ACTION_TYPES)
          .filter(([key, d]) => {
            if (row && !d.triggers.has(row.trigger)) return false;
            if (row && d.thread && !auto.T_THREADY.has(row.trigger) && !hasThread) return false;
            return matches(key, d.label);
          })
          .map(([key, d]) => ({
            name: d.reward && !isRewardAvailable(d.reward, settings) ? `${d.label} (module off)` : d.label,
            value: key,
          }));
      }
      return await respondAutocomplete(interaction, choices);
    }

    if (focused.name === 'index') {
      if (!row) return await respondAutocomplete(interaction, []);
      const needle = q.trim().toLowerCase();
      const settings = group === 'action' ? await getSettingsSafe(guildId) : null;
      const items = group === 'condition'
        ? row.conditions.map((c) => describeCondition(c, { guild: interaction.guild }))
        : row.actions.map((a) => describeAction(a, { guild: interaction.guild, settings }));
      return await respondAutocomplete(interaction, items
        .map((text, i) => ({ name: `${i + 1}. ${text}`, value: i + 1 }))
        .filter((c) => !needle || c.name.toLowerCase().includes(needle)));
    }

    return await respondAutocomplete(interaction, []);
  } catch (err) {
    logger.debug(`automations: autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}
