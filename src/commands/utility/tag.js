// /tag: custom commands, the MEE6 `!name` commands (pkg-custom-commands.md §4.1). Anyone can run, list and
// inspect a command; creating and changing them needs ADMIN (subcommandPermissions). The command is gated by
// the Custom Commands module, so the router refuses every subcommand while the module is off. Management
// replies are ephemeral and never ping; execution lives in tagService.runTag, shared with the prefix hook.
import { SlashCommandBuilder, ChannelType, AttachmentBuilder } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import prisma from '../../database/client.js';
import {
  getSettings, getSettingsSafe, peekSettings, ensureGuildSettings, isModuleEnabled,
} from '../../services/settingsService.js';
import {
  COOLDOWN_MAX_SECONDS, DESCRIPTION_MAX, EMBED_TEXT_MAX, PLAIN_MAX, TAG_ACCESS_MAX, TagError,
  actionLabel, buildUsage, clearTagCooldowns, codeSafe, cooldownLabel, createTagRow, describeAction,
  effectivePrefix, findTag, formatIdList, getEffectiveActions, invalidateTagIndex, legacyActions, normalizeActions,
  normalizeTagName, oneLine, parseArgs, primaryMessageIndex, runTag, syncLegacyColumns, validateNewTagName, writeTag,
} from '../../services/tagService.js';
import { placeholderHelp, renderPlaceholdersDetailed } from '../../utils/placeholderParser.js';
import {
  successEmbed, errorEmbed, infoEmbed, warnEmbed, applyGuildColor,
} from '../../utils/embedBuilder.js';
import {
  respondAutocomplete, checkAssignableRole, checkChannelUsable, missingBotPermissions, pingableRoleIds, safeReply,
} from '../../utils/interactionHelpers.js';
import { paginate } from '../../utils/paginator.js';
import { parseDuration } from '../../utils/parseDuration.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { memberChannelProblem, roleHierarchyProblem } from '../../utils/privacyGuards.js';
import { TimeoutError, ackBudgetMs, withTimeout } from '../../utils/async.js';
import logger from '../../utils/logger.js';

export const module = 'customcommands';
export const gated = true; // registry: gated yes (R101/R251): the router refuses every subcommand while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER; // /tag use, info, raw, list, placeholders, action list
export const subcommandPermissions = Object.freeze({
  create: PERMISSION_LEVELS.ADMIN, edit: PERMISSION_LEVELS.ADMIN, delete: PERMISSION_LEVELS.ADMIN,
  toggle: PERMISSION_LEVELS.ADMIN, cooldown: PERMISSION_LEVELS.ADMIN, permissions: PERMISSION_LEVELS.ADMIN,
  'action add': PERMISSION_LEVELS.ADMIN, 'action remove': PERMISSION_LEVELS.ADMIN, 'action list': PERMISSION_LEVELS.MEMBER,
  'response add': PERMISSION_LEVELS.ADMIN, 'response remove': PERMISSION_LEVELS.ADMIN,
});
// allowInDMs: not exported (guild only). No setDefaultMemberPermissions: members must see /tag use.

const NAME = (o) => o.setName('name').setDescription('Command name').setRequired(true).setAutocomplete(true).setMaxLength(50);
const ACCESS_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildVoice, ChannelType.GuildCategory];

export const data = new SlashCommandBuilder()
  .setName('tag')
  .setDescription("Custom commands (MEE6 !name): run, create and manage this server's commands.")
  .addSubcommand((s) => s.setName('use').setDescription('Run a custom command.')
    .addStringOption((o) => NAME(o))
    .addStringOption((o) => o.setName('args').setDescription('Arguments for {1}, {2}, … separated by spaces').setMaxLength(1000)))
  .addSubcommand((s) => s.setName('create').setDescription('Create a custom command (admin).')
    .addStringOption((o) => o.setName('name').setDescription('Name: a-z, 0-9, - and _ (used as !name)').setRequired(true).setMaxLength(32))
    .addStringOption((o) => o.setName('content').setDescription('Reply text; supports {user}, {1} arguments and other variables').setRequired(true).setMaxLength(4000))
    .addBooleanOption((o) => o.setName('embed').setDescription('Reply as an embed (up to 4000 characters)'))
    .addStringOption((o) => o.setName('description').setDescription('Shown in /tag list').setMaxLength(100)))
  .addSubcommand((s) => s.setName('edit').setDescription('Edit a custom command (admin).')
    .addStringOption((o) => NAME(o))
    .addStringOption((o) => o.setName('content').setDescription('New reply text').setMaxLength(4000))
    .addBooleanOption((o) => o.setName('embed').setDescription('Reply as an embed'))
    .addStringOption((o) => o.setName('description').setDescription('New description ("-" removes it)').setMaxLength(100))
    .addBooleanOption((o) => o.setName('delete_usage').setDescription('Delete the !name message after it runs')))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a custom command (admin).').addStringOption((o) => NAME(o)))
  .addSubcommand((s) => s.setName('toggle').setDescription('Enable or disable a custom command (admin).').addStringOption((o) => NAME(o)))
  .addSubcommand((s) => s.setName('info').setDescription("Show a custom command's settings and usage.").addStringOption((o) => NAME(o)))
  .addSubcommand((s) => s.setName('raw').setDescription('Get the exact text of a custom command.').addStringOption((o) => NAME(o)))
  .addSubcommand((s) => s.setName('list').setDescription("List this server's custom commands."))
  .addSubcommand((s) => s.setName('placeholders').setDescription('Show the variables and {1} arguments you can use.'))
  .addSubcommand((s) => s.setName('cooldown').setDescription('Set a cooldown: none, server-wide or per user (admin).')
    .addStringOption((o) => NAME(o))
    .addStringOption((o) => o.setName('scope').setDescription('Who the cooldown applies to').setRequired(true)
      .addChoices({ name: 'None', value: 'NONE' }, { name: 'Server-wide', value: 'SERVER' }, { name: 'Per user', value: 'USER' }))
    .addStringOption((o) => o.setName('duration').setDescription('e.g. 30s, 10m, 1d (max 365d)').setMaxLength(32)))
  .addSubcommand((s) => s.setName('permissions').setDescription('Allow or deny roles and channels (admin).')
    .addStringOption((o) => NAME(o))
    .addStringOption((o) => o.setName('rule').setDescription('What to do').setRequired(true)
      .addChoices({ name: 'Allow', value: 'allow' }, { name: 'Deny', value: 'deny' }, { name: 'Remove from lists', value: 'remove' }, { name: 'Clear all lists', value: 'clear' }))
    .addRoleOption((o) => o.setName('role').setDescription('Role'))
    .addChannelOption((o) => o.setName('channel').setDescription('Channel or category').addChannelTypes(...ACCESS_CHANNEL_TYPES)))
  .addSubcommandGroup((g) => g.setName('action').setDescription('What a custom command does (MEE6 action templates).')
    .addSubcommand((s) => s.setName('add').setDescription('Add an action (admin).')
      .addStringOption((o) => NAME(o))
      .addStringOption((o) => o.setName('type').setDescription('Action').setRequired(true).addChoices(
        { name: 'Reply in channel', value: 'reply' }, { name: 'Reply by DM', value: 'dm' }, { name: 'Send to a channel', value: 'send' },
        { name: 'Give a role', value: 'addRole' }, { name: 'Remove a role', value: 'removeRole' }, { name: 'Toggle a role', value: 'toggleRole' }))
      .addStringOption((o) => o.setName('text').setDescription('Message text (reply, DM, send)').setMaxLength(4000))
      .addChannelOption((o) => o.setName('channel').setDescription('Target channel (send)').addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addRoleOption((o) => o.setName('role').setDescription('Role (give, remove, toggle)'))
      .addBooleanOption((o) => o.setName('embed').setDescription('Send the text as an embed'))
      .addRoleOption((o) => o.setName('ping_role').setDescription('Let the text ping this role')))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove an action (admin).')
      .addStringOption((o) => NAME(o))
      .addIntegerOption((o) => o.setName('action').setDescription('Action number').setRequired(true).setMinValue(1).setMaxValue(LIMITS.TAG_ACTIONS).setAutocomplete(true)))
    .addSubcommand((s) => s.setName('list').setDescription("List a command's actions.").addStringOption((o) => NAME(o))))
  .addSubcommandGroup((g) => g.setName('response').setDescription('Random replies (MEE6 Add a message).')
    .addSubcommand((s) => s.setName('add').setDescription('Add a random reply to a message action (admin).')
      .addStringOption((o) => NAME(o))
      .addStringOption((o) => o.setName('text').setDescription('Reply text').setRequired(true).setMaxLength(4000))
      .addIntegerOption((o) => o.setName('action').setDescription('Action number (default: first message action)').setMinValue(1).setMaxValue(LIMITS.TAG_ACTIONS).setAutocomplete(true)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a random reply (admin).')
      .addStringOption((o) => NAME(o))
      .addIntegerOption((o) => o.setName('action').setDescription('Action number').setRequired(true).setMinValue(1).setMaxValue(LIMITS.TAG_ACTIONS).setAutocomplete(true))
      .addIntegerOption((o) => o.setName('reply').setDescription('Reply number').setRequired(true).setMinValue(1).setMaxValue(LIMITS.TAG_RESPONSES).setAutocomplete(true))));

// ── Helpers ───────────────────────────────────────────────────────────────────

const NO_MENTIONS = Object.freeze({ parse: [] });
const LIST_PAGE_SIZE = 15;
const ephemeralNote = { ephemeral: true };

/** Replies to a deferred (or fresh) interaction with embeds that never ping. */
const respond = (interaction, embed, extra = {}) => safeReply(interaction, {
  embeds: [embed], allowedMentions: NO_MENTIONS, ephemeral: true, ...extra,
});

const fail = (interaction, err) => respond(interaction, errorEmbed(err.title, err.message));

const notFoundText = (name) => `No custom command named \`${codeSafe(clampText(name, 50))}\` exists here.`;
const notFound = (interaction, name, { hint = false } = {}) => respond(
  interaction,
  errorEmbed('Not Found', `${notFoundText(name)}${hint ? ' See `/tag list`.' : ''}`),
);

/**
 * A command that gives or takes a role at or above the admin's highest role cannot be changed, enabled or disabled by that admin
 * (the guild owner is exempt). Wraps the mutator of writeTag, so it checks the row as it is stored when the write happens.
 */
const guarded = (interaction, mutator) => (tag) => {
  const ids = getEffectiveActions(tag).map((a) => a.roleId).filter(Boolean);
  const problem = roleHierarchyProblem(interaction.guild, interaction.member, ids);
  if (problem) {
    throw new TagError('HIERARCHY', 'Role Not Allowed', `${problem}\nThis command gives or takes a role at or above your highest role, so you cannot change it.`);
  }
  return mutator(tag);
};

const cleanDescription = (s) => {
  const t = clampText(String(s ?? '').trim(), DESCRIPTION_MAX);
  return t === '' ? null : t;
};

/** The explanation when `text` exceeds the limit of plain or embed replies, or null. */
function lengthProblem(text, embed) {
  const limit = embed ? EMBED_TEXT_MAX : PLAIN_MAX;
  if (text.length <= limit) return null;
  return embed
    ? `Embed replies can be at most 4,000 characters (this one is ${text.length}).`
    : `Plain-text replies can be at most 2,000 characters (this one is ${text.length}). Use \`embed:True\` for up to 4,000.`;
}

/** `⚠️ Unknown variables (shown as typed): …` for tokens no variable engine knows, or null. */
function unknownVariablesLine(texts, interaction) {
  const seen = [];
  for (const t of texts) {
    const { unknownTokens } = renderPlaceholdersDetailed(t, {
      member: interaction.member, user: interaction.user, guild: interaction.guild, channel: interaction.channel, args: [],
    });
    for (const tok of unknownTokens) if (!seen.includes(tok)) seen.push(tok);
  }
  if (!seen.length) return null;
  return `⚠️ Unknown variables (shown as typed): ${joinWithLimit(seen.map((t) => `\`${codeSafe(t)}\``), { max: 400 })}`;
}

/** The invocation text for embeds: the prefix form, or the slash form when prefix commands are off. */
const shownName = (prefix, name) => (prefix === null ? `/tag use ${name}` : `${prefix}${name}`);

const PREFIX_OFF_NOTE = 'Prefix commands are off (`/settings prefix`); run it with `/tag use`.';

/** Members never see disabled commands (/tag list and the autocomplete hide them too); admins can inspect and edit them. */
const hiddenFromMember = (tag, interaction, settings) => tag.enabled === false
  && resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN;

const timeTag = (date, style = 'R') => `<t:${Math.floor(new Date(date).getTime() / 1000)}:${style}>`;

// ── Execute ───────────────────────────────────────────────────────────────────

const HANDLERS = {
  use: useCommand,
  create,
  edit,
  delete: remove,
  toggle,
  info,
  raw,
  list,
  placeholders,
  cooldown,
  permissions,
  'action add': actionAdd,
  'action remove': actionRemove,
  'action list': actionList,
  'response add': responseAdd,
  'response remove': responseRemove,
};

// `use` and `placeholders` answer with a normal reply (the 3-second window is spent on a bounded lookup);
// every other subcommand defers first.
const NO_DEFER = new Set(['use', 'placeholders']);

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  const handler = HANDLERS[key];
  if (!handler) return undefined;
  const { guildId } = interaction;
  let settings;
  if (NO_DEFER.has(key)) {
    settings = peekSettings(guildId);
  } else {
    await interaction.deferReply(ephemeralNote);
    settings = await getSettings(guildId); // the router already loaded it (memo hit)
  }
  try {
    return await handler(interaction, settings);
  } catch (err) {
    if (err instanceof TagError) return fail(interaction, err);
    throw err; // the router's generic error reply
  }
}

// ── use ───────────────────────────────────────────────────────────────────────

async function useCommand(interaction, settings) {
  const { guild, guildId } = interaction;
  const rawName = interaction.options.getString('name', true);
  const name = normalizeTagName(rawName);
  if (!name) return notFound(interaction, rawName, { hint: true });

  let tag;
  try {
    tag = await withTimeout(findTag(guildId, name), ackBudgetMs(interaction), 'tag lookup');
  } catch (err) {
    if (err instanceof TimeoutError) {
      return respond(interaction, warnEmbed('Temporarily Unavailable', 'I could not load that command in time. Please try again.'));
    }
    throw err;
  }
  if (!tag) return notFound(interaction, name, { hint: true });
  if (!tag.enabled) return respond(interaction, errorEmbed('Disabled', 'This custom command is turned off.'));

  const args = parseArgs(interaction.options.getString('args') ?? '');
  return runTag(tag, {
    source: 'slash',
    interaction,
    guild,
    channel: interaction.channel,
    member: interaction.member,
    user: interaction.user,
    args,
    settings,
    prefix: effectivePrefix(settings?.customPrefix),
  });
}

// ── create (R030, R099, R103, R254) ───────────────────────────────────────────

async function create(interaction, settings) {
  const { guildId, options } = interaction;
  const v = validateNewTagName(options.getString('name', true));
  if (!v.ok) return respond(interaction, errorEmbed('Invalid Name', v.reason));
  const content = options.getString('content', true);
  if (content.trim() === '') return respond(interaction, errorEmbed('Empty', 'The reply text cannot be empty.'));
  const embed = options.getBoolean('embed') ?? false;
  const problem = lengthProblem(content, embed);
  if (problem) return respond(interaction, errorEmbed('Too Long', problem));
  const description = cleanDescription(options.getString('description'));

  await ensureGuildSettings(guildId);
  let row;
  try {
    row = await createTagRow({
      guildId, name: v.name, content, useEmbed: embed, description, createdBy: interaction.user.id,
    });
  } catch (err) {
    if (err?.code === 'P2002') {
      return respond(interaction, errorEmbed('Already Exists', `A custom command named \`${v.name}\` already exists. Use \`/tag edit\` to change it.`));
    }
    throw err; // TagError('LIMIT') is rendered by execute()
  }
  invalidateTagIndex(guildId);

  const prefix = effectivePrefix(settings?.customPrefix);
  const lines = [prefix === null
    ? `Run it with \`/tag use name:${v.name}\`. ${PREFIX_OFF_NOTE}`
    : `Run it with \`${prefix}${v.name}\` or \`/tag use name:${v.name}\`.`];
  const usage = buildUsage(row, { prefix });
  if (usage !== `${prefix ?? ''}${v.name}` && usage !== `/tag use name:${v.name}`) lines.push(`Usage: \`${usage}\``);
  const unknown = unknownVariablesLine([content], interaction);
  if (unknown) lines.push(unknown);
  lines.push('More: /tag action add · /tag permissions · /tag cooldown');
  return respond(interaction, successEmbed('Custom Command Created', lines.join('\n')));
}

// ── edit (R030) ───────────────────────────────────────────────────────────────

async function edit(interaction) {
  const { guild, guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const content = options.getString('content');
  const embed = options.getBoolean('embed');
  const description = options.getString('description');
  const deleteUsage = options.getBoolean('delete_usage');
  if ([content, embed, description, deleteUsage].every((x) => x === null || x === undefined)) {
    return respond(interaction, errorEmbed('Nothing to Change', 'Give at least one of content, embed, description or delete_usage.'));
  }
  if (content !== null && content.trim() === '') return respond(interaction, errorEmbed('Empty', 'The reply text cannot be empty.'));

  const row = await writeTag(guildId, name, guarded(interaction, (tag) => {
    const out = {};
    if (content !== null || embed !== null) {
      const actions = normalizeActions(tag.actions);
      if (actions === null) {
        // Legacy tag: the text and the embed flag live in the columns.
        const useEmbed = embed ?? tag.useEmbed;
        const text = content ?? tag.content;
        const problem = lengthProblem(text, useEmbed);
        if (problem) throw new TagError('TOO_LONG', 'Too Long', problem);
        out.content = text;
        out.useEmbed = useEmbed;
      } else {
        const i = primaryMessageIndex(actions);
        if (i < 0) throw new TagError('NO_MESSAGE', 'No Message Action', 'This command has no message action. Add one with `/tag action add`.');
        const copy = actions.map((a) => ({ ...a, ...(a.responses ? { responses: [...a.responses] } : {}) }));
        if (content !== null) copy[i].responses[0] = content;
        if (embed !== null) copy[i].embed = embed;
        const limit = copy[i].embed ? EMBED_TEXT_MAX : PLAIN_MAX;
        copy[i].responses.forEach((r, k) => {
          if (r.length > limit) {
            throw new TagError('TOO_LONG', 'Too Long',
              `Reply ${k + 1} of action ${i + 1} is ${r.length} characters; ${copy[i].embed ? 'embed text allows 4,000' : 'plain text allows 2,000'}.`);
          }
        });
        out.actions = copy;
        Object.assign(out, syncLegacyColumns(copy));
      }
    }
    if (description !== null) out.description = description === '-' ? null : cleanDescription(description);
    if (deleteUsage !== null) out.deleteInvocation = deleteUsage;
    return out;
  }));

  const lines = [];
  if (content !== null) lines.push('Reply text updated.');
  if (embed !== null) lines.push(`Embed: **${embed ? 'on' : 'off'}**.`);
  if (description !== null) lines.push(description === '-' ? 'Description removed.' : 'Description updated.');
  if (deleteUsage !== null) lines.push(`Delete usage: **${deleteUsage ? 'on' : 'off'}**.`);
  if (deleteUsage === true && missingBotPermissions(guild, 'ManageMessages').length) {
    lines.push('⚠️ I need **Manage Messages** to delete usages; without it the message stays.');
  }
  if (content !== null) {
    const unknown = unknownVariablesLine([content], interaction);
    if (unknown) lines.push(unknown);
  }
  return respond(interaction, successEmbed('Custom Command Updated', `\`${row.name}\`\n${lines.join('\n')}`));
}

// ── delete / toggle ───────────────────────────────────────────────────────────

async function remove(interaction) {
  const { guildId } = interaction;
  const rawName = interaction.options.getString('name', true);
  const tag = await findTag(guildId, rawName);
  if (!tag) return notFound(interaction, normalizeTagName(rawName) || rawName);
  try {
    await prisma.customTag.delete({ where: { id: tag.id } });
  } catch (err) {
    if (err?.code === 'P2025') return notFound(interaction, tag.name);
    throw err;
  }
  await clearTagCooldowns(guildId, tag.id);
  invalidateTagIndex(guildId);
  return respond(interaction, successEmbed('Custom Command Deleted', `\`${tag.name}\` (${Number(tag.uses).toLocaleString('en-US')} uses) was deleted.`));
}

async function toggle(interaction) {
  const { guildId } = interaction;
  const name = normalizeTagName(interaction.options.getString('name', true));
  if (!name) return notFound(interaction, interaction.options.getString('name', true));
  const row = await writeTag(guildId, name, guarded(interaction, (t) => ({ enabled: !t.enabled })));
  return respond(interaction, successEmbed(
    row.enabled ? 'Custom Command Enabled' : 'Custom Command Disabled',
    `\`${row.name}\` is now **${row.enabled ? 'on' : 'off'}**.`,
  ));
}

// ── cooldown ──────────────────────────────────────────────────────────────────

async function cooldown(interaction) {
  const { guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const scope = options.getString('scope', true);
  const duration = options.getString('duration');

  let patch;
  let note = '';
  if (scope === 'NONE') {
    patch = { cooldownScope: 'NONE', cooldownSeconds: 0 };
    if (duration) note = ' (duration ignored)';
  } else {
    if (!duration) return respond(interaction, errorEmbed('Duration Needed', 'Give a duration such as 30s, 10m, 2h or 1d.'));
    const ms = parseDuration(duration);
    if (ms === null || ms < 1000) {
      return respond(interaction, errorEmbed('Invalid Duration', "I couldn't read that. Use something like 30s, 10m, 2h or 1d (at least 1 second)."));
    }
    if (ms > COOLDOWN_MAX_SECONDS * 1000) return respond(interaction, errorEmbed('Too Long', 'The longest cooldown is 365 days.'));
    patch = { cooldownScope: scope, cooldownSeconds: Math.round(ms / 1000) };
  }
  const row = await writeTag(guildId, name, guarded(interaction, () => patch));
  await clearTagCooldowns(guildId, row.id);
  return respond(interaction, successEmbed('Cooldown Updated', `\`${row.name}\`: **${cooldownLabel(row)}**${note}`));
}

// ── permissions (allow and deny lists) ────────────────────────────────────────

async function permissions(interaction, settings) {
  const { guild, guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const rule = options.getString('rule', true);
  const role = options.getRole('role');
  const channel = options.getChannel('channel');

  let mutator;
  if (rule === 'clear') {
    mutator = () => ({ allowRoleIds: [], denyRoleIds: [], allowChannelIds: [], denyChannelIds: [] });
  } else {
    if (!role && !channel) return respond(interaction, errorEmbed('Nothing Selected', 'Pick a role, a channel, or both.'));
    if (role?.id === guildId) {
      return respond(interaction, errorEmbed('Not Allowed',
        '@everyone cannot be used here. Use /tag toggle to turn the command off, or rule: Clear all lists to remove limits.'));
    }
    mutator = (tag) => {
      // Arrays are copied first: settings rows are frozen and tag rows are treated as immutable too.
      const lists = {
        allowRoleIds: [...(tag.allowRoleIds ?? [])],
        denyRoleIds: [...(tag.denyRoleIds ?? [])],
        allowChannelIds: [...(tag.allowChannelIds ?? [])],
        denyChannelIds: [...(tag.denyChannelIds ?? [])],
      };
      const apply = (id, allowKey, denyKey) => {
        const without = (key) => { lists[key] = lists[key].filter((x) => x !== id); };
        const put = (key) => { if (!lists[key].includes(id)) lists[key].push(id); };
        if (rule === 'allow') { put(allowKey); without(denyKey); }
        else if (rule === 'deny') { put(denyKey); without(allowKey); }
        else { without(allowKey); without(denyKey); }
      };
      if (role) apply(role.id, 'allowRoleIds', 'denyRoleIds');
      if (channel) apply(channel.id, 'allowChannelIds', 'denyChannelIds');
      for (const list of Object.values(lists)) {
        if (list.length > TAG_ACCESS_MAX) {
          throw new TagError('LIMIT', 'Limit Reached', `A list can hold at most ${TAG_ACCESS_MAX} entries.`);
        }
      }
      return lists;
    };
  }
  const row = await writeTag(guildId, name, guarded(interaction, mutator));
  const embed = applyGuildColor(successEmbed('Permissions Updated', `\`${row.name}\``), settings);
  embed.addFields(
    { name: 'Allowed roles', value: formatIdList(row.allowRoleIds, { kind: 'role', guild, empty: 'Everyone' }), inline: true },
    { name: 'Denied roles', value: formatIdList(row.denyRoleIds, { kind: 'role', guild, empty: 'None' }), inline: true },
    { name: 'Allowed channels', value: formatIdList(row.allowChannelIds, { kind: 'channel', guild, empty: 'Everywhere' }), inline: true },
    { name: 'Denied channels', value: formatIdList(row.denyChannelIds, { kind: 'channel', guild, empty: 'None' }), inline: true },
  );
  return respond(interaction, embed);
}

// ── action add / remove / list ────────────────────────────────────────────────

async function actionAdd(interaction) {
  const { guild, guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const type = options.getString('type', true);
  const text = options.getString('text');
  const channel = options.getChannel('channel');
  const role = options.getRole('role');
  const embedOpt = options.getBoolean('embed');
  const pingRole = options.getRole('ping_role');
  const embed = embedOpt ?? false;

  // 1. Option sanity: options that do not belong to the action are refused, never ignored.
  const wrong = (message) => respond(interaction, errorEmbed('Wrong Option', message));
  if (type === 'reply' || type === 'dm') {
    if (!text || text.trim() === '') return respond(interaction, errorEmbed('Missing Text', 'This action needs `text`.'));
    if (role || channel) return wrong('`role`/`channel` are not used by this action.');
    if (type === 'dm' && pingRole) return wrong('DMs never ping roles.');
  } else if (type === 'send') {
    if (!text || text.trim() === '' || !channel) return respond(interaction, errorEmbed('Missing Option', 'This action needs `text` and `channel`.'));
    if (role) return wrong('`role` is not used by this action.');
  } else {
    if (!role) return respond(interaction, errorEmbed('Missing Role', 'This action needs `role`.'));
    if (text !== null || channel || embedOpt !== null || pingRole) {
      return wrong('`text`, `channel`, `embed` and `ping_role` are not used by role actions.');
    }
  }

  let action;
  if (type === 'addRole' || type === 'removeRole' || type === 'toggleRole') {
    // 4. Role actions: the executor must be allowed to hand out the role too.
    const r = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
    if (!r.ok) return respond(interaction, errorEmbed('Role Not Allowed', r.reason));
    action = { type, roleId: role.id };
  } else {
    // 2. Text length.
    const problem = lengthProblem(text, embed);
    if (problem) return respond(interaction, errorEmbed('Too Long', problem));
    // 3. The send channel.
    if (type === 'send') {
      const usable = checkChannelUsable(channel, {
        needs: embed ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages'],
      });
      if (!usable.ok) return respond(interaction, errorEmbed('Missing Permissions', usable.reason ?? `I cannot post in <#${channel.id}>.`));
      // The admin must be able to post there too: a command is not a way into a channel the admin cannot use.
      const access = memberChannelProblem(guild.channels.cache.get(channel.id) ?? channel, interaction.member);
      if (access) return respond(interaction, errorEmbed('No Access', access));
    }
    // 5. ping_role (R031/R049/R169): the executor needs Mention Everyone, or the role must be mentionable.
    if (pingRole) {
      if (pingRole.id === guildId) return respond(interaction, errorEmbed('Not Allowed', '@everyone can never be pinged by a custom command.'));
      const target = type === 'send' ? channel : interaction.channel;
      const { refused } = pingableRoleIds(target, interaction.member, [pingRole.id]);
      if (refused.length) {
        return respond(interaction, errorEmbed('Not Allowed',
          'You need the **Mention @everyone, @here and All Roles** permission to make a command ping a role that is not mentionable.'));
      }
    }
    // 6. Build the action.
    if (type === 'send') action = { type: 'send', channelId: channel.id, embed, responses: [text] };
    else if (type === 'dm') action = { type: 'reply', dm: true, embed, responses: [text] };
    else action = { type: 'reply', dm: false, embed, responses: [text] };
    if (pingRole && type !== 'dm') action.pingRoleIds = [pingRole.id];
  }

  // 7. Write.
  let count = 0;
  await writeTag(guildId, name, guarded(interaction, (tag) => {
    const list = normalizeActions(tag.actions) ?? legacyActions(tag);
    if (list.length >= LIMITS.TAG_ACTIONS) {
      throw new TagError('LIMIT', 'Limit Reached', `A custom command can have at most ${LIMITS.TAG_ACTIONS} actions.`);
    }
    const next = [...list, action];
    count = next.length;
    return { actions: next, ...syncLegacyColumns(next) };
  }));

  // 8. Reply.
  const lines = [`Action ${count}: ${describeAction(action, guild)}`];
  if (text) {
    const unknown = unknownVariablesLine([text], interaction);
    if (unknown) lines.push(unknown);
  }
  return respond(interaction, successEmbed('Action Added', lines.join('\n')));
}

async function actionRemove(interaction) {
  const { guild, guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const n = options.getInteger('action', true);
  let removed = null;
  await writeTag(guildId, name, guarded(interaction, (tag) => {
    const list = normalizeActions(tag.actions) ?? legacyActions(tag);
    if (n > list.length) throw new TagError('NOT_FOUND', 'Not Found', `This command has ${list.length} action(s).`);
    if (list.length === 1) {
      throw new TagError('LAST', 'Last Action', 'A custom command needs at least one action. Delete the command with /tag delete instead.');
    }
    removed = list[n - 1];
    const next = list.filter((_, i) => i !== n - 1);
    return { actions: next, ...syncLegacyColumns(next) };
  }));
  return respond(interaction, successEmbed('Action Removed', `Removed: ${describeAction(removed, guild)}`));
}

async function actionList(interaction, settings) {
  const { guild, guildId } = interaction;
  const rawName = interaction.options.getString('name', true);
  const tag = await findTag(guildId, rawName);
  if (!tag || hiddenFromMember(tag, interaction, settings)) return notFound(interaction, normalizeTagName(rawName) || rawName);
  const lines = [];
  getEffectiveActions(tag).forEach((a, i) => {
    lines.push(`\`${i + 1}.\` ${describeAction(a, guild)}`);
    if (a.responses) {
      a.responses.forEach((r, k) => lines.push(`↳ \`${k + 1}\` ${codeSafe(clampText(oneLine(r), 80))}`));
    }
  });
  return respond(interaction, applyGuildColor(
    infoEmbed(`Actions of ${tag.name}`, joinWithLimit(lines, { sep: '\n', max: 4000 })),
    settings,
  ));
}

// ── response add / remove (random replies) ────────────────────────────────────

async function responseAdd(interaction) {
  const { guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const text = options.getString('text', true);
  if (text.trim() === '') return respond(interaction, errorEmbed('Empty', 'The reply text cannot be empty.'));
  const action = options.getInteger('action');
  let where = 0;
  let count = 0;
  await writeTag(guildId, name, guarded(interaction, (tag) => {
    const list = normalizeActions(tag.actions) ?? legacyActions(tag);
    const i = action ? action - 1 : primaryMessageIndex(list);
    if (i < 0 || i >= list.length || !list[i].responses) {
      throw new TagError('NO_MESSAGE', 'No Message Action', 'Pick a reply, DM or send action.');
    }
    if (list[i].responses.length >= LIMITS.TAG_RESPONSES) {
      throw new TagError('LIMIT', 'Limit Reached', `A message action can have at most ${LIMITS.TAG_RESPONSES} replies.`);
    }
    const problem = lengthProblem(text, list[i].embed === true);
    if (problem) throw new TagError('TOO_LONG', 'Too Long', problem);
    const next = list.map((a) => ({ ...a, ...(a.responses ? { responses: [...a.responses] } : {}) }));
    next[i].responses.push(text);
    where = i + 1;
    count = next[i].responses.length;
    return { actions: next, ...syncLegacyColumns(next) };
  }));
  const lines = [`Action ${where} now has ${count} replies; one is picked at random each time.`];
  const unknown = unknownVariablesLine([text], interaction);
  if (unknown) lines.push(unknown);
  return respond(interaction, successEmbed('Reply Added', lines.join('\n')));
}

async function responseRemove(interaction) {
  const { guildId, options } = interaction;
  const name = normalizeTagName(options.getString('name', true));
  if (!name) return notFound(interaction, options.getString('name', true));
  const n = options.getInteger('action', true);
  const k = options.getInteger('reply', true);
  let left = 0;
  await writeTag(guildId, name, guarded(interaction, (tag) => {
    const list = normalizeActions(tag.actions) ?? legacyActions(tag);
    const a = list[n - 1];
    if (!a?.responses) throw new TagError('NOT_FOUND', 'Not Found', `Action ${n} is not a message action.`);
    if (k > a.responses.length) throw new TagError('NOT_FOUND', 'Not Found', `Action ${n} has ${a.responses.length} reply(ies).`);
    if (a.responses.length === 1) {
      throw new TagError('LAST', 'Last Reply', 'This is the only reply of the action. Remove the action with /tag action remove instead.');
    }
    const next = list.map((x) => ({ ...x, ...(x.responses ? { responses: [...x.responses] } : {}) }));
    next[n - 1].responses.splice(k - 1, 1);
    left = next[n - 1].responses.length;
    return { actions: next, ...syncLegacyColumns(next) };
  }));
  return respond(interaction, successEmbed('Reply Removed', `Removed reply ${k} of action ${n}; ${left} left.`));
}

// ── info ──────────────────────────────────────────────────────────────────────

async function info(interaction, settings) {
  const { guild, guildId } = interaction;
  const rawName = interaction.options.getString('name', true);
  const tag = await findTag(guildId, rawName);
  if (!tag || hiddenFromMember(tag, interaction, settings)) return notFound(interaction, normalizeTagName(rawName) || rawName);

  const prefix = effectivePrefix(settings?.customPrefix);
  const usageLines = [];
  if (prefix !== null) usageLines.push(`\`${buildUsage(tag, { prefix })}\``);
  usageLines.push(`\`${buildUsage(tag, { slash: true })}\``);
  if (prefix === null) usageLines.push(PREFIX_OFF_NOTE);

  const access = [];
  const addAccess = (label, ids, kind) => {
    if (ids?.length) access.push(`${label}: ${formatIdList(ids, { kind, guild, max: 240 })}`);
  };
  addAccess('Allowed roles', tag.allowRoleIds, 'role');
  addAccess('Denied roles', tag.denyRoleIds, 'role');
  addAccess('Allowed channels', tag.allowChannelIds, 'channel');
  addAccess('Denied channels', tag.denyChannelIds, 'channel');

  const actionLines = getEffectiveActions(tag).map((a, i) => `${i + 1}. ${describeAction(a, guild)}`);

  const embed = applyGuildColor(
    infoEmbed(`🏷️ ${shownName(prefix, tag.name)}`, tag.description ?? '*No description*'),
    settings,
  );
  embed.addFields(
    { name: 'Usage', value: clampText(usageLines.join('\n'), 1024) },
    { name: 'Status', value: tag.enabled ? '✅ Enabled' : '⏸️ Disabled', inline: true },
    { name: 'Uses', value: Number(tag.uses).toLocaleString('en-US'), inline: true },
    { name: 'Created by', value: `<@${tag.createdBy}>`, inline: true },
    { name: 'Cooldown', value: cooldownLabel(tag), inline: true },
    { name: 'Delete usage', value: tag.deleteInvocation ? 'Yes (prefix only)' : 'No', inline: true },
    { name: 'Access', value: clampText(access.length ? access.join(' · ') : 'Everyone, everywhere', 1024) },
    { name: 'Actions', value: joinWithLimit(actionLines, { sep: '\n', max: 1024 }) || '—' },
    { name: 'Created', value: timeTag(tag.createdAt), inline: true },
    { name: 'Updated', value: timeTag(tag.updatedAt), inline: true },
  );
  embed.setFooter({ text: `ID: ${tag.id}` });
  return respond(interaction, embed);
}

// ── raw (R252, R253) ──────────────────────────────────────────────────────────

async function raw(interaction, settings) {
  const { guildId } = interaction;
  const rawName = interaction.options.getString('name', true);
  const tag = await findTag(guildId, rawName);
  if (!tag || hiddenFromMember(tag, interaction, settings)) return notFound(interaction, normalizeTagName(rawName) || rawName);

  const text = tag.content ?? '';
  const actions = normalizeActions(tag.actions);
  // Room for the extra line when the actions JSON is attached too (the message limit is 2000).
  const inlineMax = actions ? 1840 : 1900;
  const files = [];
  let content;
  if (text.length <= inlineMax && !text.includes('```')) {
    // No escaping: Discord shows single backticks literally inside a code block.
    content = `**Raw text of \`${tag.name}\`:**\n\`\`\`\n${text}\n\`\`\``;
  } else {
    content = `**Raw text of \`${tag.name}\`** is attached (${text.length} characters).`;
    files.push(new AttachmentBuilder(Buffer.from(text, 'utf8'), { name: `${tag.name}.txt` }));
  }
  if (actions) {
    files.push(new AttachmentBuilder(Buffer.from(JSON.stringify(actions, null, 2), 'utf8'), { name: `${tag.name}-actions.json` }));
    content += '\nEvery action and reply is in the attached JSON.';
  }
  return safeReply(interaction, { content, files, allowedMentions: NO_MENTIONS, ephemeral: true });
}

// ── list ──────────────────────────────────────────────────────────────────────

async function list(interaction, settings) {
  const { guildId } = interaction;
  const isAdmin = resolvePermissionLevel(interaction.member, settings) >= PERMISSION_LEVELS.ADMIN;
  const rows = await prisma.customTag.findMany({
    where: { guildId, ...(isAdmin ? {} : { enabled: true }) },
    orderBy: [{ uses: 'desc' }, { name: 'asc' }],
    select: {
      id: true, guildId: true, name: true, description: true, content: true, uses: true, enabled: true, useEmbed: true,
      actions: true, cooldownScope: true, allowRoleIds: true, denyRoleIds: true, allowChannelIds: true, denyChannelIds: true,
    },
  });
  if (!rows.length) {
    return respond(interaction, infoEmbed(
      'No Custom Commands',
      isAdmin ? 'Create one with `/tag create`.' : 'This server has no custom commands yet.',
    ));
  }
  const prefix = effectivePrefix(settings?.customPrefix);
  const lines = rows.map((r) => {
    const flags = [];
    if (!r.enabled) flags.push('⏸️');
    if (r.useEmbed) flags.push('🖼️');
    if (r.cooldownScope !== 'NONE') flags.push('⏳');
    if (r.allowRoleIds.length || r.denyRoleIds.length || r.allowChannelIds.length || r.denyChannelIds.length) flags.push('🔒');
    const n = getEffectiveActions(r).length;
    if (n > 1) flags.push(`⚙️×${n}`);
    const uses = Number(r.uses);
    return clampText(
      `\`${prefix ?? ''}${r.name}\` — ${clampText(oneLine(r.description ?? r.content), 60)} · ${uses.toLocaleString('en-US')} use${uses === 1 ? '' : 's'}${flags.length ? ` ${flags.join(' ')}` : ''}`,
      150,
    );
  });
  const footer = prefix === null
    ? 'Prefix commands are off · /tag use name:<name>'
    : `Prefix: ${prefix} · /tag use name:<name>`;
  const pages = [];
  for (let i = 0; i < lines.length; i += LIST_PAGE_SIZE) {
    pages.push(applyGuildColor(
      infoEmbed(`🏷️ Custom Commands (${rows.length})`, lines.slice(i, i + LIST_PAGE_SIZE).join('\n'), footer),
      settings,
    ));
  }
  if (pages.length === 1) return interaction.editReply({ embeds: [pages[0]], allowedMentions: NO_MENTIONS });
  return paginate(interaction, pages, { ephemeral: true });
}

// ── placeholders ──────────────────────────────────────────────────────────────

async function placeholders(interaction, settings) {
  const prefix = effectivePrefix(settings?.customPrefix);
  const embed = applyGuildColor(
    infoEmbed('Custom Command Variables', clampText(placeholderHelp(['user', 'server', 'channel']), 4096)),
    settings,
  );
  embed.addFields(
    {
      name: 'Arguments',
      value: clampText([
        '`{1}` first argument (required: missing → usage reply)',
        '`{1:text}` first argument, or "text" when missing',
        '`{2...}` argument 2 to the end · `{...3}` arguments 1–3',
        '`{2...4}` arguments 2–4 · `{...}` all arguments',
      ].join('\n'), 1024),
    },
    { name: 'Random', value: '`{random}` 0–10 · `{random:1:6}` a number from 1 to 6' },
  );
  embed.setFooter({
    text: prefix === null
      ? 'Run with /tag use name:<name> args:arg1 arg2 (prefix commands are off)'
      : `Run with ${prefix}name arg1 arg2 or /tag use name:<name> args:arg1 arg2`,
  });
  return safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS, ephemeral: true });
}

// ── Autocomplete (R102, R251) ─────────────────────────────────────────────────

export async function autocomplete(interaction) {
  try {
    const { guild, guildId } = interaction;
    const settings = await getSettingsSafe(guildId);
    if (!isModuleEnabled(settings, 'customcommands')) return respondAutocomplete(interaction, []);

    const group = interaction.options.getSubcommandGroup(false);
    const sub = interaction.options.getSubcommand(false);
    const focused = interaction.options.getFocused(true);

    if (focused.name === 'name') {
      const q = normalizeTagName(focused.value);
      // Members never see disabled commands.
      const memberView = sub === 'use' || (group === null && ['info', 'raw'].includes(sub)) || (group === 'action' && sub === 'list');
      const onlyEnabled = sub === 'use'
        || (memberView && resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN);
      const rows = await prisma.customTag.findMany({
        where: {
          guildId,
          ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}),
          ...(onlyEnabled ? { enabled: true } : {}),
        },
        orderBy: [{ uses: 'desc' }, { name: 'asc' }],
        take: 25,
        select: { name: true, description: true, enabled: true },
      });
      return respondAutocomplete(interaction, rows.map((r) => ({
        name: `${r.enabled ? '' : '⏸️ '}${r.name}${r.description ? ` — ${r.description}` : ''}`,
        value: r.name,
      })));
    }

    if (focused.name === 'action' || focused.name === 'reply') {
      const tag = await findTag(guildId, normalizeTagName(interaction.options.getString('name') ?? ''));
      if (!tag) return respondAutocomplete(interaction, []);
      const actions = getEffectiveActions(tag);
      if (focused.name === 'action') {
        const messagesOnly = group === 'response' && sub === 'add';
        const choices = [];
        actions.forEach((a, i) => {
          if (messagesOnly && !a.responses) return;
          choices.push({ name: `${i + 1}. ${actionLabel(a, guild)}`, value: i + 1 });
        });
        return respondAutocomplete(interaction, choices);
      }
      const n = interaction.options.getInteger('action');
      const a = n ? actions[n - 1] : null;
      return respondAutocomplete(interaction, (a?.responses ?? []).map((r, k) => ({
        name: `${k + 1}. ${clampText(oneLine(r), 90)}`,
        value: k + 1,
      })));
    }
    return respondAutocomplete(interaction, []);
  } catch (err) {
    logger.debug(`/tag autocomplete failed: ${err?.message ?? err}`);
    return respondAutocomplete(interaction, []);
  }
}
