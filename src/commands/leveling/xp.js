// /xp (pkg-levels.md §5.2): admin XP management (MEE6 /give-xp and /remove-xp). Every write is an atomic SQL statement
// and goes through handleLevelChange, so reward roles and the `levelUp` hook stay in sync (R196, R299, R300).
// Admin changes never post a level-up announcement; the reply shows the level change instead.
import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS, XP_MAX, xpForLevel } from '../../config/constants.js';
import { runHooks } from '../../handlers/hookRegistry.js';
import { getSettings } from '../../services/settingsService.js';
import {
  ROLE_GIVE_MAX_MEMBERS, applyXpDelta, endBulk, getRank, giveXpToMembers, handleLevelChange, reportBulkResult,
  levelFromXp, resetXp, setXp, subtractXp, syncLevelRoles, tryStartBulk, xpIntoLevel,
} from '../../services/xpService.js';
import { fetchAllMembers } from '../../utils/guildMembers.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { awaitConfirm } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { formatNumber } from '../../utils/text.js';

export const module = 'leveling';
export const gated = false; // works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('xp')
  .setDescription('Manage member XP (MEE6 /give-xp, /remove-xp).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
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
  );

const reply = (interaction, embed, extra = {}) => interaction.editReply({ embeds: [embed], ...extra });
const fail = (interaction, title, text) => reply(interaction, errorEmbed(title, text));
const mention = (ids) => ids.map((id) => `<@&${id}>`).join(', ');

/** Level change and reward-role lines appended to an admin reply. */
function changeLines(result, lc) {
  const lines = [];
  if (result.newLevel !== result.oldLevel) lines.push(`Level ${result.oldLevel} → ${result.newLevel}.`);
  const roles = lc?.roles;
  if (roles?.added.length) lines.push(`Roles added: ${mention(roles.added)}.`);
  if (roles?.removed.length) lines.push(`Roles removed: ${mention(roles.removed)}.`);
  if (roles?.failed.length) lines.push(`Could not change: ${roles.failed.map((f) => `<@&${f.roleId}>`).join(', ')} (check my role position and Manage Roles).`);
  return lines.length ? `\n${lines.join('\n')}` : '';
}

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });
  const { guild, guildId } = interaction;
  const sub = interaction.options.getSubcommand();
  const settings = await getSettings(guildId);

  if (sub === 'give') return subGive(interaction, client, settings);

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
      `Removed **${formatNumber(r.oldXp - r.xp)} XP** from ${who}. Total: **${formatNumber(r.xp)} XP** (level ${r.newLevel}).${changeLines(r, lc)}`,
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
      `${who} now has **${formatNumber(r.xp)} XP** (level ${r.newLevel}).${changeLines(r, lc)}`,
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
        if (roles.removed.length) removed = `\nRoles removed: ${mention(roles.removed)}.`;
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

// ── /xp give ──────────────────────────────────────────────────────────────────

async function subGive(interaction, client, settings) {
  const { guild, guildId } = interaction;
  const amount = interaction.options.getInteger('amount', true);
  const opt = interaction.options.get('target', true);
  if (opt.role) return giveToRole(interaction, client, settings, opt.role, amount);

  const user = opt.user;
  if (!user) return fail(interaction, 'Not Saved', 'Pick a member or a role.');
  if (user.bot) return fail(interaction, 'Not Saved', "Bots don't earn XP.");
  const member = opt.member ?? await guild.members.fetch(user.id).catch(() => null);
  if (!member) return fail(interaction, 'Not Saved', "That user isn't in this server.");

  const r = await applyXpDelta({ guildId, userId: user.id, delta: amount });
  const lc = await handleLevelChange({
    guild, member, userId: user.id, result: r, settings, source: 'admin', channel: interaction.channel, announce: false,
  });
  let text = `Gave **${formatNumber(amount)} XP** to <@${user.id}>. Total: **${formatNumber(r.xp)} XP** (level ${r.newLevel}).${changeLines(r, lc)}`;
  if (r.xp === XP_MAX) text += `\nXP is capped at ${formatNumber(XP_MAX)}.`;
  return reply(interaction, successEmbed('XP Given', text));
}

/** Non-bot ids that hold `role`, from the guild's member cache. */
const humanIds = (role) => role.members.filter((m) => !m.user.bot).map((m) => m.id);

async function giveToRole(interaction, client, settings, role, amount) {
  const { guild, guildId } = interaction;
  if (role.id === guildId) return fail(interaction, 'Not Saved', "Pick a specific role; @everyone isn't allowed.");
  if (!tryStartBulk(guildId)) {
    return fail(interaction, 'Busy', 'A bulk XP or role task is already running for this server. Try again when it finishes.');
  }
  let backgroundStarted = false;
  try {
    const tooMany = (n) => `That role has ${n} members; the limit is ${formatNumber(ROLE_GIVE_MAX_MEMBERS)}. Use a smaller role.`;
    // The cap is checked before the (expensive) full member fetch, against the members already cached.
    let ids = humanIds(role);
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
      ids = humanIds(role);
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
    if (res.levelUps.length) text += '\nSyncing their reward roles in the background…';
    await reply(interaction, successEmbed('XP Given', text), { components: [] });

    if (res.levelUps.length) {
      backgroundStarted = true;
      void syncRoleLevelUps({ interaction, client, settings, levelUps: res.levelUps });
    }
    return undefined;
  } finally {
    if (!backgroundStarted) endBulk(guildId);
  }
}

/** Background half of a role give: sequential role sync and `levelUp` emission, then a summary. No announcements. */
async function syncRoleLevelUps({ interaction, client, settings, levelUps }) {
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
    await reportBulkResult(interaction, `XP give: reward roles checked for ${synced} of ${levelUps.length} member(s) who levelled up.`, { ephemeral: true });
  } catch (err) {
    logger.error('levels: role-give background task failed', err instanceof Error ? err : new Error(String(err)));
  } finally {
    endBulk(guildId);
  }
}
