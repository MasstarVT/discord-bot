// /levelconfig (pkg-levels.md §5.1): Levels settings: XP range, cooldown, multiplier, level-up announcements, voice XP,
// the rank card, role rewards, XP boost roles and no-XP exemptions. Writes go through settingsService only.
// Dispatch is group-first (R002): the top-level subcommand names never collide with the groups' subcommands.
// The `xp` group (pkg-levels.md §5.2, MEE6 /give-xp and /remove-xp) manages member XP: every write is an atomic SQL
// statement and goes through handleLevelChange, so reward roles and the `levelUp` hook stay in sync (R196, R299, R300).
// Admin changes never post a level-up announcement; the reply shows the level change instead. It answers privately.
import { AttachmentBuilder, ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';
import { LIMITS, PERMISSION_LEVELS, XP_MAX, xpForLevel } from '../../config/constants.js';
import {
  ensureGuildSettings, getSettings, updateSettings, updateSettingsWith,
} from '../../services/settingsService.js';
import { runHooks } from '../../handlers/hookRegistry.js';
import {
  ANNOUNCE_MODES, ANNOUNCE_MODE_LABELS, BULK_SYNC_MAX_MEMBERS, DEFAULT_LEVELUP_MESSAGE, ROLE_GIVE_MAX_MEMBERS,
  applyXpDelta, endBulk, getRank, giveXpToMembers, handleLevelChange, levelFromXp, levelUpVars, normalizeBoostRoles,
  reportBulkResult, resetXp, runBulkRoleSync, setXp, subtractXp, syncLevelRoles, totalXpForLevel, tryStartBulk, xpIntoLevel,
} from '../../services/xpService.js';
import {
  clearCardBackground, generateProfileCard, loadCardBackground, normalizeCardStyle, validateCardBackground,
} from '../../utils/profileCard.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { formatColor, isHttpUrl, parseColor } from '../../utils/customEmbed.js';
import {
  applyGuildColor, errorEmbed, guildEmbed, infoEmbed, successEmbed,
} from '../../utils/embedBuilder.js';
import { fetchAllMembers } from '../../utils/guildMembers.js';
import { awaitConfirm, checkAssignableRole, checkChannelUsable, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import { paginate } from '../../utils/paginator.js';
import { renderPlaceholdersDetailed } from '../../utils/placeholderParser.js';
import { safeFetchImage } from '../../utils/safeFetch.js';
import { clampText, formatNumber, joinWithLimit } from '../../utils/text.js';

export const module = 'leveling';
export const gated = false; // works while the module is off, so an admin can turn it on
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const NOXP_CHANNEL_TYPES = [
  ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice, ChannelType.GuildStageVoice,
  ChannelType.GuildForum, ChannelType.GuildMedia, ChannelType.GuildCategory,
];

export const data = new SlashCommandBuilder()
  .setName('levelconfig')
  .setDescription('Levels settings: XP, level-up messages, role rewards, rank card (MEE6 Levels).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Turn the Levels module on.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn the Levels module off. XP data is kept.'))
  .addSubcommand((s) => s.setName('status').setDescription('Show the current Levels configuration.'))
  .addSubcommand((s) =>
    s.setName('range')
      .setDescription('Set the random XP per message (MEE6 default 15–25).')
      .addIntegerOption((o) => o.setName('min').setDescription('Lowest XP per message').setRequired(true).setMinValue(1).setMaxValue(1000))
      .addIntegerOption((o) => o.setName('max').setDescription('Highest XP per message').setRequired(true).setMinValue(1).setMaxValue(1000))
  )
  .addSubcommand((s) =>
    s.setName('cooldown')
      .setDescription('Set the seconds between XP-earning messages (MEE6: 60).')
      .addIntegerOption((o) => o.setName('seconds').setDescription('Cooldown in seconds').setRequired(true).setMinValue(5).setMaxValue(3600))
  )
  .addSubcommand((s) =>
    s.setName('multiplier')
      .setDescription('Set the server-wide XP multiplier.')
      .addNumberOption((o) => o.setName('value').setDescription('Multiplier, e.g. 2 = double XP').setRequired(true).setMinValue(0.1).setMaxValue(10))
  )
  .addSubcommand((s) =>
    s.setName('announce')
      .setDescription('Choose where level-up messages are sent.')
      .addStringOption((o) =>
        o.setName('mode').setDescription('Where to announce').setRequired(true)
          .addChoices(...ANNOUNCE_MODES.map((value) => ({ name: ANNOUNCE_MODE_LABELS[value], value })))
      )
      .addChannelOption((o) =>
        o.setName('channel').setDescription('Channel for the Custom channel mode')
          .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)
      )
      .addStringOption((o) => newChannelOption(o))
      .addBooleanOption((o) => o.setName('embed').setDescription('Send as an embed instead of plain text'))
  )
  .addSubcommand((s) =>
    s.setName('message')
      .setDescription('Set the level-up message. Leave text empty to restore the default.')
      .addStringOption((o) =>
        o.setName('text').setDescription('Supports {player} {level} {rank} {xp} {user.name} {server}').setMaxLength(1000)
      )
  )
  .addSubcommand((s) =>
    s.setName('voicexp')
      .setDescription('Turn voice XP on or off and set the XP per minute.')
      .addBooleanOption((o) => o.setName('enabled').setDescription('Earn XP in voice channels').setRequired(true))
      .addIntegerOption((o) => o.setName('rate').setDescription('XP per minute').setMinValue(1).setMaxValue(50))
  )
  .addSubcommand((s) =>
    s.setName('card')
      .setDescription('Customise the /rank card colours and background.')
      .addStringOption((o) => o.setName('color').setDescription('Background colour, e.g. #2b2d31').setMaxLength(16))
      .addStringOption((o) => o.setName('accent').setDescription('Accent colour (ring, level, bar)').setMaxLength(16))
      .addStringOption((o) => o.setName('text_color').setDescription('Main text colour').setMaxLength(16))
      .addStringOption((o) => o.setName('background_url').setDescription('Background image URL (https), or "none"').setMaxLength(500))
      .addNumberOption((o) => o.setName('overlay').setDescription('Darkening over the image, 0–0.9').setMinValue(0).setMaxValue(0.9))
      .addBooleanOption((o) => o.setName('reset').setDescription('Restore the default card'))
  )
  .addSubcommand((s) =>
    s.setName('demotion-sync')
      .setDescription('Remove reward roles when a member drops below their level.')
      .addBooleanOption((o) => o.setName('enabled').setDescription('Remove roles on demotion').setRequired(true))
  )
  .addSubcommandGroup((g) =>
    g.setName('noxp')
      .setDescription('Roles and channels that earn no XP.')
      .addSubcommand((s) =>
        s.setName('role')
          .setDescription('Add or remove a no-XP role.')
          .addRoleOption((o) => o.setName('role').setDescription('Role').setRequired(true))
          .addBooleanOption((o) => o.setName('remove').setDescription('Remove it from the list'))
      )
      .addSubcommand((s) =>
        s.setName('channel')
          .setDescription('Add or remove a no-XP channel or category.')
          .addChannelOption((o) =>
            o.setName('channel').setDescription('Channel or category').setRequired(true).addChannelTypes(...NOXP_CHANNEL_TYPES)
          )
          .addBooleanOption((o) => o.setName('remove').setDescription('Remove it from the list'))
      )
  )
  .addSubcommandGroup((g) =>
    g.setName('role')
      .setDescription('Level role rewards.')
      .addSubcommand((s) =>
        s.setName('add')
          .setDescription('Give a role when members reach a level.')
          .addIntegerOption((o) => o.setName('level').setDescription('Level').setRequired(true).setMinValue(1).setMaxValue(999))
          .addRoleOption((o) => o.setName('role').setDescription('Reward role'))
          .addStringOption((o) => newRoleOption(o))
          .addBooleanOption((o) => o.setName('sync').setDescription('Also give it to members already at this level (default: yes)'))
      )
      .addSubcommand((s) =>
        s.setName('remove')
          .setDescription('Remove the reward for a level.')
          .addIntegerOption((o) => o.setName('level').setDescription('Level').setRequired(true).setMinValue(1).setAutocomplete(true))
      )
      .addSubcommand((s) => s.setName('list').setDescription('List the role rewards.'))
      .addSubcommand((s) =>
        s.setName('stack')
          .setDescription('Keep lower rewards (stack) or only the highest (replace).')
          .addBooleanOption((o) => o.setName('enabled').setDescription('true = stack, false = replace').setRequired(true))
      )
      .addSubcommand((s) =>
        s.setName('sync')
          .setDescription('Re-sync reward roles for one member or everyone ranked.')
          .addUserOption((o) => o.setName('user').setDescription('Only this member'))
      )
  )
  .addSubcommandGroup((g) =>
    g.setName('boost')
      .setDescription('XP boost roles (the highest multiplier applies).')
      .addSubcommand((s) =>
        s.setName('add')
          .setDescription('Add or update an XP boost role.')
          .addRoleOption((o) => o.setName('role').setDescription('Role').setRequired(true))
          .addNumberOption((o) => o.setName('multiplier').setDescription('Boost, e.g. 1.5 = +50%').setRequired(true).setMinValue(1.1).setMaxValue(5))
      )
      .addSubcommand((s) =>
        s.setName('remove')
          .setDescription('Remove an XP boost role.')
          .addRoleOption((o) => o.setName('role').setDescription('Role').setRequired(true))
      )
      .addSubcommand((s) => s.setName('list').setDescription('List the XP boost roles.'))
  )
  .addSubcommandGroup((g) =>
    g.setName('xp')
      .setDescription('Manage member XP (MEE6 /give-xp, /remove-xp).')
      .addSubcommand((s) =>
        s.setName('give')
          .setDescription('Give XP to a member, or to every member of a role.')
          .addMentionableOption((o) => o.setName('target').setDescription('Member or role').setRequired(true))
          .addIntegerOption((o) => o.setName('amount').setDescription('XP to give').setRequired(true).setMinValue(1).setMaxValue(1_000_000))
      )
      .addSubcommand((s) =>
        s.setName('remove')
          .setDescription('Remove XP from a member.')
          .addUserOption((o) => o.setName('user').setDescription('Member').setRequired(true))
          .addIntegerOption((o) => o.setName('amount').setDescription('XP to remove').setRequired(true).setMinValue(1).setMaxValue(1_000_000))
      )
      .addSubcommand((s) =>
        s.setName('set')
          .setDescription("Set a member's total XP.")
          .addUserOption((o) => o.setName('user').setDescription('Member').setRequired(true))
          .addIntegerOption((o) => o.setName('amount').setDescription('New total XP').setRequired(true).setMinValue(0).setMaxValue(1_000_000_000))
      )
      .addSubcommand((s) =>
        s.setName('reset')
          .setDescription("Reset a member's XP and level to zero.")
          .addUserOption((o) => o.setName('user').setDescription('Member').setRequired(true))
      )
      .addSubcommand((s) =>
        s.setName('view')
          .setDescription("View a member's XP data.")
          .addUserOption((o) => o.setName('user').setDescription('Member').setRequired(true))
      )
  );

/** A validation failure inside a settings mutator; execute() turns it into an error reply and nothing is saved. */
class ConfigError extends Error {}

const reply = (interaction, embed, extra = {}) => interaction.editReply({ embeds: [embed], ...extra });
const fail = (interaction, title, text) => reply(interaction, errorEmbed(title, text));

/** True for Discord CDN upload links, which are signed and stop working after a while. Its static art (avatars, emoji) never expires. */
export function isExpiringDiscordLink(url) {
  let parsed;
  try { parsed = new URL(url); } catch { return false; }
  const host = parsed.hostname.toLowerCase();
  if (host !== 'cdn.discordapp.com' && host !== 'media.discordapp.net') return false;
  return /^\/(ephemeral-)?attachments\//i.test(parsed.pathname);
}
const voiceKeys = (guildId) => `voice:start:${guildId}:*`;
const roleLine = (roleId) => `<@&${roleId}>`;
const listOr = (ids, fmt) => (ids.length ? ids.map(fmt).join(', ') : 'none');

// ── Entry points ──────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const group = interaction.options.getSubcommandGroup(false);
  // The XP management group answers privately; every other subcommand answers publicly.
  if (group === 'xp') return executeXp(interaction, client);
  await interaction.deferReply();
  const sub = interaction.options.getSubcommand();
  try {
    if (!(await resolveNewOptions(interaction, data, { required: ['role'] }))) return undefined;
    if (group === 'noxp') return await (sub === 'role' ? subNoxpRole(interaction) : subNoxpChannel(interaction));
    if (group === 'role') {
      switch (sub) {
        case 'add': return await subRoleAdd(interaction);
        case 'remove': return await subRoleRemove(interaction);
        case 'list': return await subRoleList(interaction);
        case 'stack': return await subRoleStack(interaction);
        case 'sync': return await subRoleSync(interaction);
        default: return undefined;
      }
    }
    if (group === 'boost') {
      switch (sub) {
        case 'add': return await subBoostAdd(interaction);
        case 'remove': return await subBoostRemove(interaction);
        case 'list': return await subBoostList(interaction);
        default: return undefined;
      }
    }
    switch (sub) {
      case 'enable': return await subEnable(interaction);
      case 'disable': return await subDisable(interaction);
      case 'status': return await subStatus(interaction);
      case 'range': return await subRange(interaction);
      case 'cooldown': return await subCooldown(interaction);
      case 'multiplier': return await subMultiplier(interaction);
      case 'announce': return await subAnnounce(interaction);
      case 'message': return await subMessage(interaction);
      case 'voicexp': return await subVoiceXp(interaction);
      case 'card': return await subCard(interaction);
      case 'demotion-sync': return await subDemotionSync(interaction);
      default: return undefined;
    }
  } catch (err) {
    if (err instanceof ConfigError) return reply(interaction, errorEmbed('Not Saved', err.message));
    throw err;
  }
}

export async function autocomplete(interaction) {
  const typed = String(interaction.options.getFocused() ?? '').trim();
  const guild = interaction.guild;
  // Rule 21: the answer is due in 3 s, so the lookup is bounded; a timeout answers with no choices.
  const rows = await withTimeout(
    prisma.levelRole.findMany({ where: { guildId: interaction.guildId }, orderBy: { level: 'asc' }, take: LIMITS.LEVEL_ROLES }),
    ackBudgetMs(interaction),
    'levelconfig autocomplete',
  ).catch(() => []);
  // An integer column cannot use a database `contains`, so the prefix filter runs here.
  const choices = rows
    .filter((r) => String(r.level).startsWith(typed))
    .slice(0, 25)
    .map((r) => ({ name: `Level ${r.level} → ${guild?.roles.cache.get(r.roleId)?.name ?? 'deleted role'}`, value: r.level }));
  return respondAutocomplete(interaction, choices);
}

// ── Module and XP settings ────────────────────────────────────────────────────

async function subEnable(interaction) {
  const S = await updateSettings(interaction.guildId, { xpEnabled: true });
  return reply(interaction, successEmbed(
    'Levels Enabled',
    `Members now earn **${S.xpMin}–${S.xpMax} XP** per message (once every ${S.xpCooldown}s). Configure with \`/levelconfig\`.`,
  ));
}

async function subDisable(interaction) {
  await updateSettings(interaction.guildId, { xpEnabled: false });
  // Open voice sessions are dropped without credit, so nothing is paid after the module comes back on.
  await cache.invalidate(voiceKeys(interaction.guildId));
  return reply(interaction, successEmbed('Levels Disabled', 'XP is no longer granted. Existing XP and rewards are kept.'));
}

async function subRange(interaction) {
  const min = interaction.options.getInteger('min', true);
  const max = interaction.options.getInteger('max', true);
  if (min > max) return fail(interaction, 'Invalid Range', '`min` must be less than or equal to `max`.');
  const S = await updateSettings(interaction.guildId, { xpMin: min, xpMax: max });
  return reply(interaction, successEmbed(
    'XP Range Updated',
    `Members earn **${S.xpMin}–${S.xpMax} XP** per message (before multipliers). MEE6 uses 15–25.`,
  ));
}

async function subCooldown(interaction) {
  const S = await updateSettings(interaction.guildId, { xpCooldown: interaction.options.getInteger('seconds', true) });
  return reply(interaction, successEmbed('Cooldown Updated', `Members can earn XP once every **${S.xpCooldown}s**.`));
}

async function subMultiplier(interaction) {
  const value = Math.round(interaction.options.getNumber('value', true) * 100) / 100;
  const S = await updateSettings(interaction.guildId, { xpMultiplier: value });
  return reply(interaction, successEmbed('Multiplier Updated', `Server-wide XP multiplier: **×${S.xpMultiplier}**.`));
}

async function subVoiceXp(interaction) {
  const enabled = interaction.options.getBoolean('enabled', true);
  const rate = interaction.options.getInteger('rate');
  const data = { voiceXpEnabled: enabled };
  if (rate !== null) data.voiceXpRate = rate;
  const S = await updateSettings(interaction.guildId, data);
  if (!enabled) await cache.invalidate(voiceKeys(interaction.guildId)); // drop open sessions without credit
  let text = enabled
    ? `Voice XP **on** at **${S.voiceXpRate} XP/minute**. No XP while deafened, alone, in the AFK channel or in a no-XP channel.`
    : 'Voice XP **off**.';
  if (!S.xpEnabled) text += '\n⚠️ The Levels module is off; enable it with `/levelconfig enable`.';
  return reply(interaction, successEmbed('Voice XP Updated', text));
}

async function subDemotionSync(interaction) {
  const enabled = interaction.options.getBoolean('enabled', true);
  await updateSettings(interaction.guildId, { levelRolesSyncOnDemotion: enabled });
  return reply(interaction, successEmbed(
    'Demotion Sync Updated',
    enabled
      ? 'Members who lose XP (`/levelconfig xp remove`, `xp set`, `xp reset`) also lose reward roles above their new level.'
      : 'Reward roles are kept when members lose XP.',
  ));
}

// ── Status ────────────────────────────────────────────────────────────────────

async function subStatus(interaction) {
  const { guild, guildId } = interaction;
  const [S, rewardCount] = await Promise.all([getSettings(guildId), prisma.levelRole.count({ where: { guildId } })]);
  const boosts = normalizeBoostRoles(S.xpBoostRoles);
  const style = normalizeCardStyle(S.rankCardStyle);

  let announce = ANNOUNCE_MODE_LABELS[S.levelUpMode] ?? ANNOUNCE_MODE_LABELS.CURRENT;
  if (S.levelUpMode === 'CHANNEL') {
    announce += S.levelUpChannelId
      ? ` → <#${S.levelUpChannelId}>${guild.channels.cache.has(S.levelUpChannelId) ? '' : ' ⚠️ missing'}`
      : ' ⚠️ no channel set';
  }
  announce += S.levelUpUseEmbed ? ' · embed' : ' · text';

  const message = clampText((S.levelUpMessage ?? DEFAULT_LEVELUP_MESSAGE).replaceAll('```', 'ˋˋˋ'), 950);
  const embed = guildEmbed(S, '⭐ Levels Configuration').addFields(
    { name: 'Module', value: S.xpEnabled ? '✅ On' : '❌ Off — /levelconfig enable' },
    { name: 'XP per message', value: `${S.xpMin}–${S.xpMax}`, inline: true },
    { name: 'Cooldown', value: `${S.xpCooldown}s`, inline: true },
    { name: 'Multiplier', value: `×${S.xpMultiplier}`, inline: true },
    { name: 'Announcements', value: announce },
    { name: 'Voice XP', value: S.voiceXpEnabled ? `✅ ${S.voiceXpRate} XP/min` : '❌ Off', inline: true },
    {
      name: 'Role rewards',
      value: `${rewardCount} configured · ${S.levelRolesStack ? 'Stack' : 'Replace'} · demotion sync ${S.levelRolesSyncOnDemotion ? 'on' : 'off'}`,
    },
    { name: 'XP boost roles', value: joinWithLimit(boosts.map((b) => `${roleLine(b.roleId)} ×${b.multiplier}`), { max: 900 }) || 'None' },
    { name: 'No-XP roles', value: joinWithLimit((S.noXpRoleIds ?? []).map(roleLine), { max: 900 }) || 'None' },
    { name: 'No-XP channels', value: joinWithLimit((S.noXpChannelIds ?? []).map((id) => `<#${id}>`), { max: 900 }) || 'None' },
    {
      name: 'Rank card',
      value: `background ${style.background}${style.backgroundUrl ? ' + image' : ''} · accent ${style.accentColor} · text ${style.textColor}`,
    },
    { name: `Level-up message${S.levelUpMessage ? '' : ' (default)'}`, value: `\`\`\`\n${message}\n\`\`\`` },
  );
  return reply(interaction, embed);
}

// ── Level-up messages ─────────────────────────────────────────────────────────

async function subAnnounce(interaction) {
  const { guild, guildId } = interaction;
  const mode = interaction.options.getString('mode', true);
  const channel = interaction.options.getChannel('channel');
  const embed = interaction.options.getBoolean('embed'); // null = unchanged
  const settings = await getSettings(guildId);

  if (channel && mode !== 'CHANNEL') {
    return fail(interaction, 'Not Saved', 'The `channel` option is only used with **Custom channel** mode.');
  }
  const data = { levelUpMode: mode };
  if (mode === 'CHANNEL') {
    const target = channel ?? (settings.levelUpChannelId ? guild.channels.cache.get(settings.levelUpChannelId) : null);
    if (!target) return fail(interaction, 'Channel Required', 'Pick a channel for **Custom channel** mode.');
    const needs = (embed ?? settings.levelUpUseEmbed) ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages'];
    const usable = checkChannelUsable(target, { needs });
    if (!usable.ok) {
      return fail(interaction, 'Missing Permissions', `I can't post in <#${target.id}>: ${usable.reason || `missing ${usable.missing.join(', ')}`}.`);
    }
    data.levelUpChannelId = target.id; // other modes leave the channel alone, so switching back restores it
  }
  if (embed !== null) data.levelUpUseEmbed = embed;

  const S = await updateSettings(guildId, data);
  const where = S.levelUpMode === 'CHANNEL' && S.levelUpChannelId ? ` → <#${S.levelUpChannelId}>` : '';
  return reply(interaction, successEmbed(
    'Level-up Messages',
    `Mode: **${ANNOUNCE_MODE_LABELS[S.levelUpMode]}**${where}. Format: **${S.levelUpUseEmbed ? 'embed' : 'plain text'}**.`,
  ));
}

async function subMessage(interaction) {
  const { guild, guildId } = interaction;
  const text = interaction.options.getString('text');
  if (text === null) {
    await updateSettings(guildId, { levelUpMessage: null });
    return reply(interaction, successEmbed('Level-up Message Reset', `Restored the MEE6 default: \`${DEFAULT_LEVELUP_MESSAGE}\``));
  }
  const trimmed = text.trim();
  if (!trimmed) return fail(interaction, 'Not Saved', "The message can't be empty.");
  const S = await updateSettings(guildId, { levelUpMessage: trimmed });

  const preview = renderPlaceholdersDetailed(
    trimmed,
    {
      member: interaction.member,
      user: interaction.user,
      guild,
      channel: interaction.channel,
      vars: levelUpVars({ user: interaction.user, member: interaction.member, level: 5, xp: 1150, rank: 1 }),
    },
    { maxLength: 2000 },
  );
  let description = `Preview (sample values):\n${clampText(preview.text, 3500)}`;
  if (S.levelUpMode === 'OFF') description += '\n\nAnnouncements are currently **disabled**; turn them on with `/levelconfig announce`.';
  const embed = successEmbed('Level-up Message Updated', description);
  if (preview.unknownTokens.length) {
    embed.addFields({
      name: '⚠️ Unknown variables',
      value: `${joinWithLimit(preview.unknownTokens.map((t) => `\`${t}\``), { max: 900 })}\nThey will appear exactly as typed.`,
    });
  }
  embed.addFields({
    name: 'Level variables',
    value: '`{player}` mention · `{player.name}` display name · `{level}` new level · `{rank}` leaderboard position · '
      + '`{xp}` total XP · `{mention}` (old alias). All `{user.*}`, `{server.*}`, `{channel.*}` and `{random}` variables also work.',
  });
  return reply(interaction, embed);
}

// ── Rank card ─────────────────────────────────────────────────────────────────

async function cardPreview(interaction, S) {
  try {
    const { guildId, member, user } = interaction;
    const style = normalizeCardStyle(S.rankCardStyle);
    let level = 5;
    let totalXp = 1150;
    let rank = 1;
    const row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: user.id } } });
    if (row && row.xp > 0) {
      totalXp = row.xp;
      level = levelFromXp(row.xp);
      rank = await getRank(guildId, user.id, row.xp);
    }
    const avatarBuffer = await safeFetchImage((member ?? user).displayAvatarURL({ extension: 'png', size: 256 }), { maxBytes: 2 * 1024 * 1024, timeoutMs: 5000 })
      .catch(() => null);
    const backgroundBuffer = style.backgroundUrl ? await loadCardBackground(style.backgroundUrl) : null;
    const png = await generateProfileCard({
      avatarBuffer,
      username: user.username,
      displayName: member?.displayName ?? user.globalName ?? user.username,
      level,
      totalXp,
      rank,
      xpCurrent: xpIntoLevel(totalXp),
      xpNeeded: xpForLevel(level),
      style,
      backgroundBuffer,
    });
    return new AttachmentBuilder(png, { name: 'rank-preview.png' });
  } catch (err) {
    logger.warn(`levelconfig: could not render the rank card preview: ${err?.message ?? err}`);
    return null;
  }
}

const styleSummary = (style) => [
  `**Background:** ${style.background}`,
  `**Accent:** ${style.accentColor}`,
  `**Text:** ${style.textColor}`,
  `**Image:** ${style.backgroundUrl ?? 'none'}`,
  `**Overlay:** ${style.overlayOpacity}`,
].join('\n');

async function replyWithCard(interaction, embed, S) {
  const file = await cardPreview(interaction, S);
  if (!file) return reply(interaction, embed);
  return reply(interaction, embed.setImage('attachment://rank-preview.png'), { files: [file] });
}

async function subCard(interaction) {
  const { guildId } = interaction;
  const o = interaction.options;
  const input = {
    color: o.getString('color'),
    accent: o.getString('accent'),
    text: o.getString('text_color'),
    url: o.getString('background_url'),
    overlay: o.getNumber('overlay'),
  };
  const anyChange = Object.values(input).some((v) => v !== null);
  const settings = await getSettings(guildId);

  // 1. Nothing given: show the current card.
  if (o.getBoolean('reset') !== true && !anyChange) {
    return replyWithCard(interaction, applyGuildColor(infoEmbed('Rank Card', styleSummary(normalizeCardStyle(settings.rankCardStyle))), settings), settings);
  }

  // 2. Reset (any other option given alongside is ignored).
  if (o.getBoolean('reset') === true) {
    const oldUrl = normalizeCardStyle(settings.rankCardStyle).backgroundUrl;
    const S = await updateSettings(guildId, { rankCardStyle: Prisma.DbNull });
    clearCardBackground(oldUrl);
    const note = anyChange ? '\nThe other options were ignored because `reset` was set.' : '';
    return replyWithCard(interaction, successEmbed('Rank Card Reset', `Restored the default card.${note}`), S);
  }

  // 3. Colours.
  const changes = {};
  const colours = [['background', input.color], ['accentColor', input.accent], ['textColor', input.text]];
  for (const [key, value] of colours) {
    if (value === null) continue;
    const n = parseColor(value);
    if (n === null) {
      return fail(interaction, 'Invalid Colour', `\`${clampText(value, 40)}\` isn't a colour. Use a hex code like #5865F2 or a name like blurple.`);
    }
    changes[key] = formatColor(n);
  }
  if (input.overlay !== null) changes.overlayOpacity = Math.round(input.overlay * 100) / 100;

  // 4. Background image.
  let warning = '';
  if (input.url !== null) {
    const url = input.url.trim();
    if (['none', 'clear', 'remove'].includes(url.toLowerCase())) {
      changes.backgroundUrl = null;
    } else {
      if (!isHttpUrl(url)) return fail(interaction, 'Invalid URL', 'Use a direct https link to a PNG, JPG, GIF or WebP image.');
      const v = await validateCardBackground(url);
      if (!v.ok) return fail(interaction, 'Image Not Loaded', `I couldn't load an image from that link (${v.reason}).`);
      changes.backgroundUrl = url;
      if (isExpiringDiscordLink(url)) {
        warning = '\n⚠️ Discord attachment links expire after about a day. Use a permanent image host.';
      }
    }
  }

  // 5. Write. The URL was validated above, so the mutator stays pure.
  let oldUrl = null;
  const S = await updateSettingsWith(guildId, (cur) => {
    const before = normalizeCardStyle(cur.rankCardStyle);
    oldUrl = before.backgroundUrl;
    return { rankCardStyle: { ...before, ...changes } };
  });
  clearCardBackground(oldUrl);
  clearCardBackground(normalizeCardStyle(S.rankCardStyle).backgroundUrl);

  // 6. Preview.
  return replyWithCard(interaction, successEmbed('Rank Card Updated', `${styleSummary(normalizeCardStyle(S.rankCardStyle))}${warning}`), S);
}

// ── No-XP roles and channels ─────────────────────────────────────────────────

/**
 * Builds the pure mutator of a no-XP list edit. `exists` prunes entries whose role or channel is gone; it is passed only
 * while the guild is available, because an unavailable guild has empty caches and a cache miss is never a deletion
 * (00-overview rule 20).
 */
function noxpMutator({ field, id, remove, exists, cap, noun }) {
  return (cur) => {
    let list = [...(cur[field] ?? [])];
    if (exists) list = list.filter(exists);
    if (remove) {
      if (!list.includes(id)) throw new ConfigError(`That ${noun} is not on the no-XP list.`);
      return { [field]: list.filter((x) => x !== id) };
    }
    if (list.includes(id)) throw new ConfigError('Already on the no-XP list.');
    if (list.length >= cap) throw new ConfigError(`You can have at most ${cap} no-XP ${noun}s.`);
    return { [field]: [...list, id] };
  };
}

async function subNoxpRole(interaction) {
  const { guild, guildId } = interaction;
  const role = interaction.options.getRole('role', true);
  const remove = interaction.options.getBoolean('remove') ?? false;
  if (!remove && role.id === guildId) return fail(interaction, 'Not Saved', 'Use `/levelconfig disable` to stop XP for everyone.');
  await updateSettingsWith(guildId, noxpMutator({
    field: 'noXpRoleIds',
    id: role.id,
    remove,
    exists: guild.available ? (x) => guild.roles.cache.has(x) : null,
    cap: LIMITS.NOXP_ROLES,
    noun: 'role',
  }));
  return reply(interaction, successEmbed('No-XP Roles', `${roleLine(role.id)} ${remove ? 'removed from' : 'added to'} the no-XP roles.`));
}

async function subNoxpChannel(interaction) {
  const { guild, guildId } = interaction;
  const channel = interaction.options.getChannel('channel', true);
  const remove = interaction.options.getBoolean('remove') ?? false;
  await updateSettingsWith(guildId, noxpMutator({
    field: 'noXpChannelIds',
    id: channel.id,
    remove,
    exists: guild.available ? (x) => guild.channels.cache.has(x) : null,
    cap: LIMITS.NOXP_CHANNELS,
    noun: 'channel',
  }));
  return reply(interaction, successEmbed(
    'No-XP Channels',
    remove
      ? `<#${channel.id}> removed from the no-XP channels.`
      : `<#${channel.id}> added to the no-XP channels. Threads, forum posts and channels inside a no-XP category earn no XP either.`,
  ));
}

// ── Role rewards ──────────────────────────────────────────────────────────────

/** Runs a role-sync task in the background (the caller already took the bulk lock) and always releases the lock. */
function runBulkInBackground(interaction, label, load, { settings, syncDown }) {
  void (async () => {
    try {
      const rows = await load();
      const entries = rows.map((e) => ({ userId: e.userId, level: levelFromXp(e.xp) }));
      const r = await runBulkRoleSync({ guild: interaction.guild, entries, settings, syncDown });
      await reportBulkResult(interaction, `${label}: ${r.changed} updated, ${r.failed} failed, ${r.missing} no longer in the server.`, { ephemeral: false });
    } catch (err) {
      logger.error(`levels: ${label} failed`, err instanceof Error ? err : new Error(String(err)));
    } finally {
      endBulk(interaction.guildId);
    }
  })();
}

async function subRoleAdd(interaction) {
  const { guild, guildId } = interaction;
  const level = interaction.options.getInteger('level', true);
  const role = interaction.options.getRole('role', true);
  const sync = interaction.options.getBoolean('sync') ?? true;

  if (totalXpForLevel(level) > XP_MAX) {
    return fail(
      interaction,
      'Level Too High',
      `Level ${level} needs more than ${formatNumber(XP_MAX)} XP, so nobody can reach it. The highest reachable level is ${levelFromXp(XP_MAX)}.`,
    );
  }
  const check = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: true });
  if (!check.ok) return fail(interaction, "Can't Use This Role", check.reason);

  const other = await prisma.levelRole.findFirst({ where: { guildId, roleId: role.id, NOT: { level } } });
  if (other) return fail(interaction, 'Already a Reward', `${roleLine(role.id)} is already the reward for level ${other.level}. Remove that first.`);
  const existing = await prisma.levelRole.findUnique({ where: { guildId_level: { guildId, level } } });
  if (!existing && (await prisma.levelRole.count({ where: { guildId } })) >= LIMITS.LEVEL_ROLES) {
    return fail(interaction, 'Limit Reached', `You can have at most ${LIMITS.LEVEL_ROLES} role rewards.`);
  }

  await ensureGuildSettings(guildId);
  await prisma.levelRole.upsert({
    where: { guildId_level: { guildId, level } },
    update: { roleId: role.id },
    create: { guildId, level, roleId: role.id },
  });
  const S = await getSettings(guildId);

  let text = `Members who reach level **${level}** get ${roleLine(role.id)}.`;
  if (existing && existing.roleId !== role.id) text += ` (Replaces ${roleLine(existing.roleId)}.)`;
  if (check.dangerous.length) text += `\n⚠️ This role grants: ${check.dangerous.join(', ')}.`;

  let lock = false;
  if (sync) {
    lock = tryStartBulk(guildId);
    text += lock
      ? `\nGiving it to members already at level ${level}+ in the background…`
      : '\nA role sync is already running; run `/levelconfig role sync` when it finishes.';
  }
  try {
    await reply(interaction, successEmbed('Role Reward Added', text));
  } catch (err) {
    if (lock) endBulk(guildId);
    throw err;
  }
  if (lock) {
    runBulkInBackground(
      interaction,
      `Role sync for level ${level}`,
      () => prisma.userLevel.findMany({
        where: { guildId, xp: { gte: totalXpForLevel(level) } },
        orderBy: [{ xp: 'desc' }, { userId: 'asc' }],
        take: BULK_SYNC_MAX_MEMBERS,
        select: { userId: true, xp: true },
      }),
      { settings: S, syncDown: false },
    );
  }
  return undefined;
}

async function subRoleRemove(interaction) {
  const { guildId } = interaction;
  const level = interaction.options.getInteger('level', true);
  const row = await prisma.levelRole.findUnique({ where: { guildId_level: { guildId, level } } });
  if (!row) return fail(interaction, 'Not Found', `No role reward is set for level ${level}.`);
  await prisma.levelRole.deleteMany({ where: { guildId, level } });
  return reply(interaction, successEmbed(
    'Role Reward Removed',
    `Removed the level **${level}** reward (${roleLine(row.roleId)}). Members who already have the role keep it.`,
  ));
}

async function subRoleList(interaction) {
  const { guild, guildId } = interaction;
  const [S, rows] = await Promise.all([
    getSettings(guildId),
    prisma.levelRole.findMany({ where: { guildId }, orderBy: { level: 'asc' } }),
  ]);
  if (!rows.length) return reply(interaction, infoEmbed('No Role Rewards', 'Add one with `/levelconfig role add`.'));

  const lines = rows.map((r) => {
    const role = guild.roles.cache.get(r.roleId);
    let line = `**Level ${r.level}** → ${roleLine(r.roleId)}`;
    if (!role) line += ' ⚠️ deleted role';
    else {
      const check = checkAssignableRole(guild, role, { allowDangerous: true });
      if (!check.ok) line += ` ⚠️ ${check.reason}`;
    }
    return line;
  });
  const footer = `${rows.length} reward(s) · ${S.levelRolesStack ? 'Stack' : 'Replace'} mode`;
  const pages = [];
  for (let i = 0; i < lines.length; i += 25) {
    pages.push(guildEmbed(S, '🎁 Level Role Rewards', lines.slice(i, i + 25).join('\n'), footer));
  }
  if (pages.length === 1) return reply(interaction, pages[0]);
  return paginate(interaction, pages);
}

async function subRoleStack(interaction) {
  const enabled = interaction.options.getBoolean('enabled', true);
  await updateSettings(interaction.guildId, { levelRolesStack: enabled });
  const what = enabled
    ? 'Members **keep** every reward role they earn.'
    : 'Members keep **only the highest** reward role they earned.';
  return reply(interaction, successEmbed('Role Reward Mode', `${what} Run \`/levelconfig role sync\` to apply this to existing members.`));
}

const rankedRows = (guildId) => prisma.userLevel.findMany({
  where: { guildId, xp: { gt: 0 } },
  orderBy: [{ xp: 'desc' }, { userId: 'asc' }],
  take: BULK_SYNC_MAX_MEMBERS,
  select: { userId: true, xp: true },
});

async function subRoleSync(interaction) {
  const { guild, guildId } = interaction;
  const user = interaction.options.getUser('user');
  const S = await getSettings(guildId);

  if (user) {
    const member = await guild.members.fetch(user.id).catch(() => null);
    if (!member) return fail(interaction, 'Not a Member', "That user isn't in this server.");
    if (user.bot) return fail(interaction, 'Not Saved', "Bots don't have levels.");
    const row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: user.id } } });
    const level = row ? levelFromXp(row.xp) : 0;
    const r = await syncLevelRoles(member, level, S, { syncDown: S.levelRolesSyncOnDemotion, reason: 'Level role sync' });
    let text = `<@${user.id}> (level ${level}): added ${listOr(r.added, roleLine)}, removed ${listOr(r.removed, roleLine)}.`;
    if (r.failed.length) text += `\nFailed: ${r.failed.map((f) => `${roleLine(f.roleId)} (${clampText(f.reason, 120)})`).join(', ')}`;
    return reply(interaction, successEmbed('Roles Synced', text));
  }

  if ((await prisma.levelRole.count({ where: { guildId } })) === 0) {
    return reply(interaction, infoEmbed('No Role Rewards', 'Add one with `/levelconfig role add`.'));
  }
  if (!tryStartBulk(guildId)) {
    return fail(interaction, 'Busy', 'A role sync is already running for this server. Try again when it finishes.');
  }
  let rows;
  try {
    rows = await rankedRows(guildId);
    let text = `Re-syncing reward roles for ${rows.length} ranked member(s) in the background. I'll post a summary here when it's done.`;
    if (rows.length >= BULK_SYNC_MAX_MEMBERS) text += `\n(Only the top ${BULK_SYNC_MAX_MEMBERS} are synced.)`;
    await reply(interaction, successEmbed('Role Sync Started', text, 'Members without XP are not checked.'));
  } catch (err) {
    endBulk(guildId);
    throw err;
  }
  runBulkInBackground(interaction, 'Role sync', async () => rows, { settings: S, syncDown: S.levelRolesSyncOnDemotion });
  return undefined;
}

// ── XP boost roles ────────────────────────────────────────────────────────────

async function subBoostAdd(interaction) {
  const { guildId } = interaction;
  const role = interaction.options.getRole('role', true);
  const multiplier = Math.round(interaction.options.getNumber('multiplier', true) * 100) / 100;
  if (role.id === guildId) return fail(interaction, 'Not Saved', 'Use `/levelconfig multiplier` to boost everyone.');
  await updateSettingsWith(guildId, (cur) => {
    const current = normalizeBoostRoles(cur.xpBoostRoles);
    if (!current.some((b) => b.roleId === role.id) && current.length >= LIMITS.XP_BOOST_ROLES) {
      throw new ConfigError(`You can have at most ${LIMITS.XP_BOOST_ROLES} XP boost roles.`);
    }
    return { xpBoostRoles: normalizeBoostRoles([...current.filter((b) => b.roleId !== role.id), { roleId: role.id, multiplier }]) };
  });
  return reply(interaction, successEmbed(
    'XP Boost Saved',
    `${roleLine(role.id)} earns **×${multiplier}** XP. When a member has several boost roles, only the highest applies.`,
  ));
}

async function subBoostRemove(interaction) {
  const role = interaction.options.getRole('role', true);
  await updateSettingsWith(interaction.guildId, (cur) => {
    const current = normalizeBoostRoles(cur.xpBoostRoles);
    if (!current.some((b) => b.roleId === role.id)) throw new ConfigError('That role is not an XP boost role.');
    return { xpBoostRoles: current.filter((b) => b.roleId !== role.id) };
  });
  return reply(interaction, successEmbed('XP Boost Removed', `${roleLine(role.id)} no longer boosts XP.`));
}

async function subBoostList(interaction) {
  const { guild, guildId } = interaction;
  const S = await getSettings(guildId);
  const boosts = normalizeBoostRoles(S.xpBoostRoles);
  if (!boosts.length) {
    return reply(interaction, applyGuildColor(infoEmbed('XP Boost Roles', 'No XP boost roles. Add one with `/levelconfig boost add`.'), S));
  }
  const lines = boosts.map((b) => `${roleLine(b.roleId)} — ×${b.multiplier}${guild.roles.cache.has(b.roleId) ? '' : ' ⚠️ deleted role'}`);
  return reply(interaction, applyGuildColor(infoEmbed('XP Boost Roles', lines.join('\n')), S));
}

// ── XP management (/levelconfig xp give | remove | set | reset | view) ───────────────────────────────────────

const mentionRoles = (ids) => ids.map((id) => `<@&${id}>`).join(', ');

/** Level change and reward-role lines appended to an admin reply. */
function xpChangeLines(result, lc) {
  const lines = [];
  if (result.newLevel !== result.oldLevel) lines.push(`Level ${result.oldLevel} → ${result.newLevel}.`);
  const roles = lc?.roles;
  if (roles?.added.length) lines.push(`Roles added: ${mentionRoles(roles.added)}.`);
  if (roles?.removed.length) lines.push(`Roles removed: ${mentionRoles(roles.removed)}.`);
  if (roles?.failed.length) lines.push(`Could not change: ${roles.failed.map((f) => `<@&${f.roleId}>`).join(', ')} (check my role position and Manage Roles).`);
  return lines.length ? `\n${lines.join('\n')}` : '';
}

async function executeXp(interaction, client) {
  await interaction.deferReply({ ephemeral: true });
  const { guild, guildId } = interaction;
  const sub = interaction.options.getSubcommand();
  const settings = await getSettings(guildId);

  if (sub === 'give') return xpGive(interaction, client, settings);

  const user = interaction.options.getUser('user', true);
  const userId = user.id;
  const who = `<@${userId}>`;

  if (sub === 'remove') {
    const amount = interaction.options.getInteger('amount', true);
    const r = await subtractXp({ guildId, userId, amount });
    if (!r || r.oldXp === 0) return reply(interaction, infoEmbed('Nothing to Remove', `${who} has no XP in this server.`));
    const member = await guild.members.fetch(userId).catch(() => null);
    const lc = await handleLevelChange({ guild, member, userId, result: r, settings, source: 'admin', announce: false });
    return reply(interaction, successEmbed(
      'XP Removed',
      `Removed **${formatNumber(r.oldXp - r.xp)} XP** from ${who}. Total: **${formatNumber(r.xp)} XP** (level ${r.newLevel}).${xpChangeLines(r, lc)}`,
    ));
  }

  if (sub === 'set') {
    const amount = interaction.options.getInteger('amount', true);
    const member = await guild.members.fetch(userId).catch(() => null);
    if (amount > 0) {
      if (user.bot) return fail(interaction, 'Not Saved', "Bots don't earn XP.");
      if (!member) return fail(interaction, 'Not Saved', "That user isn't in this server.");
    }
    const r = await setXp({ guildId, userId, xp: amount });
    if (!r) return reply(interaction, infoEmbed('Nothing to Set', `${who} already has 0 XP.`));
    const lc = await handleLevelChange({ guild, member, userId, result: r, settings, source: 'admin', announce: false });
    return reply(interaction, successEmbed(
      'XP Set',
      `${who} now has **${formatNumber(r.xp)} XP** (level ${r.newLevel}).${xpChangeLines(r, lc)}`,
    ));
  }

  if (sub === 'reset') {
    const old = await resetXp(guildId, userId);
    if (!old) return reply(interaction, infoEmbed('Nothing to Reset', `${who} has no XP in this server.`));
    let removed = '';
    if (settings.levelRolesSyncOnDemotion) {
      const member = await guild.members.fetch(userId).catch(() => null);
      if (member) {
        const roles = await syncLevelRoles(member, 0, settings, { syncDown: true, reason: 'XP reset' });
        if (roles.removed.length) removed = `\nRoles removed: ${mentionRoles(roles.removed)}.`;
      }
    }
    return reply(interaction, successEmbed('XP Reset', `Wiped XP and level for ${who} (was level ${old.oldLevel}).${removed}`));
  }

  // view
  const row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId } } });
  if (!row) return reply(interaction, infoEmbed('No Data', `${who} has no XP in this server.`));
  const [rank, member] = await Promise.all([getRank(guildId, userId, row.xp), guild.members.fetch(userId).catch(() => null)]);
  const level = levelFromXp(row.xp); // the XP is the truth; the stored level can lag during a race
  const embed = guildEmbed(settings, `XP — ${member?.displayName ?? user.username}`)
    .addFields(
      { name: 'Level', value: String(level), inline: true },
      { name: 'Total XP', value: formatNumber(row.xp), inline: true },
      { name: 'Rank', value: `#${formatNumber(rank)}`, inline: true },
      { name: 'Progress', value: `${formatNumber(xpIntoLevel(row.xp))} / ${formatNumber(xpForLevel(level))}`, inline: true },
      { name: 'Messages', value: formatNumber(row.messageCount), inline: true },
      { name: 'Voice', value: `${formatNumber(row.voiceMinutes)} min`, inline: true },
      { name: 'Last XP', value: row.lastXpAt ? `<t:${Math.floor(row.lastXpAt.getTime() / 1000)}:R>` : 'Never', inline: true },
    )
    .setThumbnail((member ?? user).displayAvatarURL());
  return reply(interaction, embed);
}

// ── /levelconfig xp give ──────────────────────────────────────────────────────

async function xpGive(interaction, client, settings) {
  const { guild, guildId } = interaction;
  const amount = interaction.options.getInteger('amount', true);
  const opt = interaction.options.get('target', true);
  if (opt.role) return xpGiveToRole(interaction, client, settings, opt.role, amount);

  const user = opt.user;
  if (!user) return fail(interaction, 'Not Saved', 'Pick a member or a role.');
  if (user.bot) return fail(interaction, 'Not Saved', "Bots don't earn XP.");
  const member = opt.member ?? await guild.members.fetch(user.id).catch(() => null);
  if (!member) return fail(interaction, 'Not Saved', "That user isn't in this server.");

  // Read first so the reply can say what the cap let through (display only; the write below stays atomic).
  const before = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: user.id } }, select: { xp: true } });
  const r = await applyXpDelta({ guildId, userId: user.id, delta: amount });
  const lc = await handleLevelChange({
    guild, member, userId: user.id, result: r, settings, source: 'admin', channel: interaction.channel, announce: false,
  });
  const added = Math.min(amount, Math.max(0, r.xp - (before?.xp ?? 0)));
  if (added === 0) {
    return reply(interaction, infoEmbed(
      'Already at the XP Cap',
      `<@${user.id}> is already at the XP cap of ${formatNumber(XP_MAX)}, so no XP was added. Total: **${formatNumber(r.xp)} XP** (level ${r.newLevel}).`,
    ));
  }
  let text = `Gave **${formatNumber(added)} XP** to <@${user.id}>. Total: **${formatNumber(r.xp)} XP** (level ${r.newLevel}).${xpChangeLines(r, lc)}`;
  if (added < amount) text += `\nOnly ${formatNumber(added)} of ${formatNumber(amount)} XP fitted: XP is capped at ${formatNumber(XP_MAX)}.`;
  else if (r.xp === XP_MAX) text += `\nXP is capped at ${formatNumber(XP_MAX)}.`;
  return reply(interaction, successEmbed('XP Given', text));
}

/** Non-bot ids that hold `role`, from the guild's member cache. */
const xpHumanIds = (role) => role.members.filter((m) => !m.user.bot).map((m) => m.id);

async function xpGiveToRole(interaction, client, settings, role, amount) {
  const { guild, guildId } = interaction;
  if (role.id === guildId) return fail(interaction, 'Not Saved', "Pick a specific role; @everyone isn't allowed.");
  if (!tryStartBulk(guildId)) {
    return fail(interaction, 'Busy', 'A bulk XP or role task is already running for this server. Try again when it finishes.');
  }
  let backgroundStarted = false;
  try {
    const tooMany = (n) => `That role has ${n} members; the limit is ${formatNumber(ROLE_GIVE_MAX_MEMBERS)}. Use a smaller role.`;
    // The cap is checked before the (expensive) full member fetch, against the members already cached.
    let ids = xpHumanIds(role);
    if (ids.length > ROLE_GIVE_MAX_MEMBERS) return await fail(interaction, 'Not Saved', tooMany(ids.length));

    if (guild.members.cache.size < guild.memberCount) {
      const res = await fetchAllMembers(guild, { timeoutMs: 60_000 });
      if (!res.ok) {
        return await fail(
          interaction,
          'Not Saved',
          res.reason === 'too-large'
            ? 'This server has too many members for me to load them all. Give XP to members one by one instead.'
            : "I couldn't load the member list right now. Try again in a minute.",
        );
      }
      ids = xpHumanIds(role);
    }
    if (ids.length === 0) return await fail(interaction, 'Not Saved', 'No members (other than bots) have that role.');
    if (ids.length > ROLE_GIVE_MAX_MEMBERS) return await fail(interaction, 'Not Saved', tooMany(ids.length));

    const ok = await awaitConfirm(interaction, {
      title: 'Give XP to a role?',
      description: `Give **${formatNumber(amount)} XP** to **${formatNumber(ids.length)}** members of <@&${role.id}>?`,
      confirmLabel: 'Give XP',
      danger: false,
    });
    if (!ok) return await reply(interaction, infoEmbed('Cancelled', 'No XP was given.'), { components: [] });

    const res = await giveXpToMembers({ guildId, userIds: ids, amount });
    let text = `Gave **${formatNumber(amount)} XP** to **${formatNumber(res.updated)}** members of <@&${role.id}>. ${res.levelUps.length} levelled up.`;
    // With no reward roles there is nothing to sync, so say nothing about it (the level-up hooks still run below).
    const hasRewards = res.levelUps.length > 0 && (await prisma.levelRole.count({ where: { guildId } }).catch(() => 1)) > 0;
    if (hasRewards) text += '\nSyncing their reward roles in the background…';
    await reply(interaction, successEmbed('XP Given', text), { components: [] });

    if (res.levelUps.length) {
      backgroundStarted = true;
      void xpSyncRoleLevelUps({ interaction, client, settings, levelUps: res.levelUps });
    }
    return undefined;
  } finally {
    if (!backgroundStarted) endBulk(guildId);
  }
}

/** Background half of a role give: sequential role sync and `levelUp` emission, then a summary. No announcements. */
async function xpSyncRoleLevelUps({ interaction, client, settings, levelUps }) {
  const { guild, guildId } = interaction;
  try {
    const rewards = await prisma.levelRole.findMany({ where: { guildId } });
    let synced = 0;
    for (const lu of levelUps) {
      try {
        const m = guild.members.cache.get(lu.userId);
        if (m && rewards.length) {
          await syncLevelRoles(m, lu.newLevel, settings, { syncDown: false, rewards });
          synced++;
        }
        await runHooks('levelUp', {
          client, guild, member: m ?? null, userId: lu.userId, oldLevel: lu.oldLevel, newLevel: lu.newLevel, channel: null, source: 'admin',
        });
      } catch (err) {
        logger.error(`levels: role-give follow-up failed for ${lu.userId}`, err instanceof Error ? err : new Error(String(err)));
      }
    }
    if (rewards.length) {
      await reportBulkResult(interaction, `XP give: reward roles checked for ${synced} of ${levelUps.length} member(s) who levelled up.`, { ephemeral: true });
    }
  } catch (err) {
    logger.error('levels: role-give background task failed', err instanceof Error ? err : new Error(String(err)));
  } finally {
    endBulk(guildId);
  }
}
