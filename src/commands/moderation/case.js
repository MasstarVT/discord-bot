import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { COLORS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  ACTION_META,
  MSG,
  createInfraction,
  deliverLookup,
  deliverModReply,
  displayType,
  postModLog,
} from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { errorEmbed, guildEmbed, infoEmbed, neutralEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { isHttpUrl } from '../../utils/customEmbed.js';
import { awaitConfirm, isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
/** Clearing warnings is an administrator action; viewing cases and histories stays with moderators. */
export const subcommandPermissions = { 'clear-history': PERMISSION_LEVELS.ADMIN };

const MAX_CASE_ID = 2_147_483_647; // infraction.caseId is a 32-bit Int column
const CASES_PER_PAGE = 5;
const MAX_ROWS = 250;
const NO_MENTIONS = { parse: [] };

const TYPE_CHOICES = [
  { name: 'Active only', value: 'active' }, { name: 'Warnings', value: 'WARN' }, { name: 'Timeouts', value: 'TIMEOUT' },
  { name: 'Mutes', value: 'MUTE' }, { name: 'Kicks', value: 'KICK' }, { name: 'Softbans', value: 'SOFTBAN' },
  { name: 'Bans', value: 'BAN' }, { name: 'Unbans', value: 'UNBAN' }, { name: 'Unmutes', value: 'UNMUTE' }, { name: 'Notes', value: 'NOTE' },
];
const TYPE_LABEL = Object.fromEntries(TYPE_CHOICES.map((c) => [c.value, c.name]));

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);

export const data = new SlashCommandBuilder()
  .setName('case')
  .setDescription("View or edit a moderation case, or list or clear a member's infractions.")
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('view').setDescription('View a case.')
    .addIntegerOption((o) => o.setName('id').setDescription('Case number').setRequired(true).setMinValue(1)))
  .addSubcommand((s) => s.setName('reason').setDescription("Change a case's reason.")
    .addIntegerOption((o) => o.setName('id').setDescription('Case number').setRequired(true).setMinValue(1))
    .addStringOption((o) => o.setName('reason').setDescription('New reason').setRequired(true).setMaxLength(1000)))
  .addSubcommand((s) => s.setName('proof').setDescription("Set a case's proof link.")
    .addIntegerOption((o) => o.setName('id').setDescription('Case number').setRequired(true).setMinValue(1))
    .addStringOption((o) => o.setName('url').setDescription('Proof link (http/https)').setRequired(true).setMaxLength(1000)))
  .addSubcommand((s) => s.setName('history').setDescription("List a member's moderation cases (MEE6 /infractions).")
    .addUserOption((o) => o.setName('user').setDescription('Member to look up').setRequired(true))
    .addStringOption((o) => o.setName('type').setDescription('Only show one kind of case').addChoices(...TYPE_CHOICES)))
  .addSubcommand((s) => s.setName('clear-history').setDescription("Pardon all of a member's active warnings (MEE6 /clear-all-infractions).")
    .addUserOption((o) => o.setName('user').setDescription('Member whose warnings to clear').setRequired(true))
    .addStringOption((o) => reasonOpt(o, 'Why the warnings are cleared')));

const notFound = (caseId) => errorEmbed('Not Found', `Case #${caseId} doesn't exist in this server.`);
const quote = (text) => String(text).replace(/\n/g, '\n> ');
const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);
const metaOf = (type) => ACTION_META[type] ?? { emoji: '•', label: String(type) };

/** "<@id> `tag`", or "Unknown" when the user cannot be loaded. */
async function personField(client, id) {
  const user = client.users.cache.get(id) ?? await client.users.fetch(id).catch(() => null);
  if (!user) return `<@${id}> \`Unknown\``;
  return `<@${id}> \`${clampText(String(user.tag ?? user.username ?? id).replace(/`/g, "'"), 100)}\``;
}

/** Best-effort note in the case-log channel; postModLog never throws, the catch is a second guard. */
async function logEdit(guild, caseId, text) {
  try {
    await postModLog(guild, neutralEmbed(`📝 Case #${caseId} edited`, clampText(text, 4000)));
  } catch (err) {
    logger.warn(`/case: mod log note failed: ${err?.message ?? err}`);
  }
}

/**
 * /case view | reason | proof | history | clear-history. Every displayed value is clamped (older rows may hold very long text),
 * and reason / proof edits are bounded to 1000 characters (R095).
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {import('discord.js').Client} [client]
 * @returns {Promise<void>}
 */
export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const sub = interaction.options.getSubcommand();
  if (sub === 'history' || sub === 'clear-history') {
    const settings = await getSettingsSafe(interaction.guildId);
    if (sub === 'clear-history') await clearWarnings(interaction, settings);
    else await viewHistory(interaction, settings);
    return;
  }

  const caseId = interaction.options.getInteger('id', true);
  const guild = interaction.guild;
  const guildId = interaction.guildId;
  const where = { guildId_caseId: { guildId, caseId } };

  if (caseId < 1 || caseId > MAX_CASE_ID) {
    await interaction.editReply({ embeds: [notFound(caseId)] });
    return;
  }

  // ── view ──────────────────────────────────────────────────────────────────
  if (sub === 'view') {
    const settings = await getSettingsSafe(guildId);
    const inf = await prisma.infraction.findUnique({ where });
    if (!inf) {
      await interaction.editReply({ embeds: [notFound(caseId)] });
      return;
    }

    const bot = client ?? interaction.client;
    const [target, moderator] = await Promise.all([personField(bot, inf.targetUserId), personField(bot, inf.moderatorId)]);
    const meta = ACTION_META[displayType(inf)] ?? { emoji: '📋', label: String(inf.type), color: COLORS.NEUTRAL };
    const embed = neutralEmbed(clampText(`${meta.emoji} Case #${caseId} | ${meta.label}`, 256)).setColor(meta.color);
    embed.addFields(
      { name: 'Target', value: target, inline: true },
      { name: 'Moderator', value: moderator, inline: true },
      { name: 'Reason', value: clampText(inf.reason || 'None', 1024) },
      { name: 'Active', value: inf.active ? 'Yes' : 'No', inline: true },
      { name: 'Created', value: discordTimestamp(inf.createdAt, 'F'), inline: true },
    );
    if (inf.expiresAt) {
      embed.addFields({ name: 'Expires', value: `${discordTimestamp(inf.expiresAt, 'F')} (${discordTimestamp(inf.expiresAt, 'R')})`, inline: true });
    }
    if (inf.proofUrl) embed.addFields({ name: 'Proof', value: clampText(inf.proofUrl, 1024) });
    embed.setFooter({ text: `User ID: ${inf.targetUserId}` });
    await deliverLookup(interaction, settings, { embeds: [embed], allowedMentions: NO_MENTIONS });
    return;
  }

  // ── reason / proof ────────────────────────────────────────────────────────
  let data;
  let confirmation;
  let logText;
  if (sub === 'reason') {
    const newReason = clampText(interaction.options.getString('reason', true).trim(), 1000);
    if (!newReason) {
      await interaction.editReply({ embeds: [errorEmbed('Invalid Input', "The reason can't be empty.")] });
      return;
    }
    data = { reason: newReason };
    confirmation = successEmbed('Case Updated', `Case #${caseId} reason is now:\n> ${quote(clampText(newReason, 1000))}`);
    logText = `Reason changed by <@${interaction.user.id}>:\n> ${quote(newReason)}`;
  } else {
    const url = interaction.options.getString('url', true).trim();
    if (!isHttpUrl(url) || url.length > 1000) {
      await interaction.editReply({ embeds: [errorEmbed('Invalid Link', MSG.BAD_PROOF)] });
      return;
    }
    data = { proofUrl: url };
    confirmation = successEmbed('Case Updated', `Case #${caseId} proof link updated.`);
    logText = `Proof link changed by <@${interaction.user.id}>:\n${url}`;
  }

  const settings = await getSettingsSafe(guildId);
  try {
    await prisma.infraction.update({ where, data });
  } catch (err) {
    if (err?.code === 'P2025') {
      await interaction.editReply({ embeds: [notFound(caseId)] });
      return;
    }
    throw err;
  }
  await deliverLookup(interaction, settings, { embeds: [confirmation], allowedMentions: NO_MENTIONS });
  await logEdit(guild, caseId, logText);
}

// ── history ───────────────────────────────────────────────────────────────────

/** Live Discord state for a member who has no records at all (hint only; a failed lookup adds no hint). */
async function liveStateHints(guild, userId) {
  const [ban, member] = await Promise.all([
    guild.bans.fetch({ user: userId, force: true, cache: false }).catch(() => null),
    guild.members.fetch(userId).catch(() => null),
  ]);
  const lines = [];
  if (ban) {
    lines.push('⚠️ **This user is currently banned**, but the ban is not in the records yet (it will be imported at the next ban sync).');
    if (ban.reason) lines.push(`Ban reason on record: ${clampText(String(ban.reason).replace(/`/g, "'"), 300)}`);
  } else if (member?.isCommunicationDisabled?.()) {
    lines.push('⚠️ **This member is currently timed out** (not by this bot).');
    lines.push(`Timeout ends: ${discordTimestamp(member.communicationDisabledUntilTimestamp, 'R')}`);
  }
  return lines;
}

async function viewHistory(interaction, settings) {
  const user = interaction.options.getUser('user', true);
  const type = interaction.options.getString('type');
  const guild = interaction.guild;
  const guildId = interaction.guildId;

  const where = { guildId, targetUserId: user.id, ...(type === 'active' ? { active: true } : type ? { type } : {}) };
  const [total, rows, counts, activeWarns] = await Promise.all([
    prisma.infraction.count({ where }),
    prisma.infraction.findMany({ where, orderBy: { caseId: 'desc' }, take: MAX_ROWS }),
    prisma.infraction.groupBy({ by: ['type'], where: { guildId, targetUserId: user.id }, _count: { _all: true } }),
    prisma.infraction.count({ where: { guildId, targetUserId: user.id, type: 'WARN', active: true } }),
  ]);

  if (rows.length === 0) {
    const lines = [`No moderation history for <@${user.id}>${type ? ` (filter: ${TYPE_LABEL[type] ?? type})` : ''}.`];
    // R246: "not in the records yet" is only true when the member has no records at all. A filter that
    // matches nothing, for someone with recorded cases, must not claim a recorded ban is missing.
    if (counts.length === 0) {
      const hints = await liveStateHints(guild, user.id);
      if (hints.length) lines.push('', ...hints);
    }
    await deliverLookup(interaction, settings, { embeds: [infoEmbed('No Records', lines.join('\n'))], allowedMentions: NO_MENTIONS });
    return;
  }

  const summary = clampText([
    [...counts]
      .sort((a, b) => b._count._all - a._count._all || String(a.type).localeCompare(String(b.type)))
      .map((c) => `${metaOf(c.type).emoji} ${c._count._all}× ${metaOf(c.type).label}`)
      .join(' · '),
    `**Active warnings:** ${activeWarns}`,
  ].filter(Boolean).join('\n'), 4096);
  const footer = `${total} record(s)${total > MAX_ROWS ? ` · showing the ${MAX_ROWS} most recent` : ''}`;
  const avatar = typeof user.displayAvatarURL === 'function' ? user.displayAvatarURL() : null;

  const pages = [];
  for (let i = 0; i < rows.length; i += CASES_PER_PAGE) {
    const embed = guildEmbed(settings, clampText(`📋 Infractions | ${user.tag ?? user.username ?? user.id}`, 256), summary, footer);
    if (avatar) embed.setThumbnail(avatar);
    for (const inf of rows.slice(i, i + CASES_PER_PAGE)) {
      const meta = metaOf(displayType(inf));
      const reason = clampText(String(inf.reason ?? 'No reason').replace(/\s*\n\s*/g, ' '), 200);
      embed.addFields({
        name: clampText(`${meta.emoji} Case #${inf.caseId} | ${meta.label}${inf.active ? ' 🔴 active' : ''}`, 256),
        value: clampText(
          `${discordTimestamp(inf.createdAt, 'D')} by <@${inf.moderatorId}>${inf.expiresAt ? ` · ends ${discordTimestamp(inf.expiresAt, 'R')}` : ''}\n> ${reason}`,
          1024,
        ),
      });
    }
    pages.push(embed);
  }

  const publicReply = (settings?.modVisibility ?? 'PUBLIC') === 'PUBLIC';
  try {
    if (publicReply) {
      // R026-R028 / R055: the deferred reply is gone after deleteReply, so the pages go out as follow-ups.
      await interaction.deleteReply().catch(() => {});
      await paginate(interaction, pages, { followUp: true });
    } else {
      await paginate(interaction, pages, { ephemeral: true }); // edits the deferred ephemeral reply
    }
  } catch (err) {
    if (isInteractionGone(err)) return;
    logger.warn(`/case history: could not send the pages: ${err?.message ?? err}`);
    const embed = errorEmbed('Action Failed', "I couldn't show the history here. Check my permissions in this channel and try again.");
    if (publicReply) await interaction.followUp({ embeds: [embed], ephemeral: true }).catch(() => {});
    else await interaction.editReply({ embeds: [embed] }).catch(() => {});
  }
}

// ── clear-history ─────────────────────────────────────────────────────────────

async function clearWarnings(interaction, settings) {
  const user = interaction.options.getUser('user', true);
  const reason = interaction.options.getString('reason');
  const guildId = interaction.guildId;
  const activeWarns = { guildId, targetUserId: user.id, type: 'WARN', active: true };

  const n = await prisma.infraction.count({ where: activeWarns });
  if (n === 0) {
    await interaction.editReply({ embeds: [infoEmbed('Nothing To Clear', `<@${user.id}> has no active warnings.`)] });
    return;
  }

  const confirmed = await awaitConfirm(interaction, {
    title: 'Clear warnings?',
    description: `This pardons **${n}** active warning(s) for <@${user.id}>. Their cases stay in the history, and escalation counting starts over.`,
    confirmLabel: 'Clear warnings',
    danger: true,
    ephemeral: true,
  });
  if (!confirmed) return; // awaitConfirm already shows the outcome

  const { count } = await prisma.infraction.updateMany({ where: activeWarns, data: { active: false } });
  const noteReason = clampText(`[Pardon] Cleared ${count} warning(s): ${reason?.trim() || 'No reason provided'}`, 1000);

  // NOTE: the pardon itself is the updateMany above; this only records it as a case (and posts the case log).
  const lines = [
    `**User:** <@${user.id}> \`${tagOf(user)}\``,
    `**Warnings cleared:** ${count}`,
    `**Reason:** ${clampText(reason?.trim() || 'No reason provided', 1000)}`,
  ];
  const warnings = [];
  try {
    const note = await createInfraction(interaction.guild, user, interaction.user, 'NOTE', {
      reason: noteReason,
      silent: true,
      skipEscalation: true,
      client: interaction.client,
    });
    lines.push(`**Case:** #${note.caseId}`);
  } catch (err) {
    logger.error(`/case clear-history: could not record the pardon note in ${guildId}`, err);
    warnings.push('The warnings were cleared, but the pardon could not be saved as a case.');
  }
  await deliverModReply(interaction, settings, { title: 'Warnings Cleared', lines, warnings });
}
