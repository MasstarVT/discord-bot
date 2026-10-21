// /liverole set | require | enable | disable | announce | status (pkg-liverole.md §4). Admin config command: replies are public (CLAUDE.md admin
// convention) and it keeps working while the Live Role module is off (gated = false). The engine lives in src/plugins/liverole.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES } from '../../config/modules.js';
import { getSettingsSafe, updateSettings } from '../../services/settingsService.js';
import { successEmbed, errorEmbed, infoEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import {
  safeDefer, safeReply, checkAssignableRole, checkChannelUsable, pingableRoleIds, awaitConfirm, isInteractionGone, permissionNames,
} from '../../utils/interactionHelpers.js';
import { renderPlaceholdersDetailed } from '../../utils/placeholderParser.js';
import { joinWithLimit, formatNumber, clampText } from '../../utils/text.js';
import { withTimeout } from '../../utils/async.js';
import { newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import { presencesEnabled } from '../../utils/gatewayFeatures.js';
import logger from '../../utils/logger.js';
import {
  LIVEROLE, DEFAULT_LIVE_ANNOUNCE_MESSAGE, isStreaming, streamLabel, countLosingHolders,
  loadMembers, syncGuild, formatSyncResult, liveRoleProblems,
} from '../../plugins/liverole.js';

export const module = 'liverole';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('liverole')
  .setDescription('Give a role while members stream (MEE6 Twitch Alerts live-streaming role).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('set').setDescription('Set the role members get while they are live.')
    .addRoleOption((o) => o.setName('role').setDescription('A dedicated role, e.g. @Live.'))
    .addStringOption((o) => newRoleOption(o)))
  .addSubcommand((s) => s.setName('require').setDescription('Only give the live role to members with a role.')
    .addRoleOption((o) => o.setName('role').setDescription('Required role (leave empty for everyone).')))
  .addSubcommand((s) => s.setName('enable').setDescription('Turn the live role on and re-sync it.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn the live role off.')
    .addBooleanOption((o) => o.setName('remove_roles').setDescription('Remove it from members who have it now (default: yes).')))
  .addSubcommand((s) => s.setName('announce').setDescription('Post a stream link when a member goes live.')
    .addChannelOption((o) => o.setName('channel').setDescription('Where to post. Leave every option empty to stop.')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
    .addStringOption((o) => o.setName('message').setDescription('Text with {url}, {platform}, {title}; "default" resets.')
      .setMaxLength(LIVEROLE.ANNOUNCE_MAX_CHARS))
    .addRoleOption((o) => o.setName('ping_role').setDescription('Role to ping in each post.'))
    .addBooleanOption((o) => o.setName('clear_ping_role').setDescription('Stop pinging a role.')))
  .addSubcommand((s) => s.setName('status').setDescription('Show the setup and who is live now.'));

// ── Shared pieces ─────────────────────────────────────────────────────────────

const MSG = {
  EVERYONE: () => errorEmbed('Invalid Role', "@everyone can't be the live role. Pick or create a dedicated role such as @Live."),
  SAME_AS_REQUIRED: () => errorEmbed('Invalid Role', "The live role can't also be the required role. Change the requirement with `/liverole require`."),
  REQUIRED_IS_LIVE: () => errorEmbed('Invalid Role', "The required role can't be the live role itself."),
  CANNOT_USE: (reason) => errorEmbed('Cannot Use That Role', `${reason} Pick another role, or fix my role position and permissions.`),
  NO_ROLE: () => errorEmbed('No Live Role Set', 'Pick the role first with `/liverole set role:@Live` (or create it with `new_role:Live`), then run `/liverole enable`.'),
  ROLE_DELETED: () => errorEmbed('Live Role Missing', 'The live role was deleted. Pick a new one with `/liverole set`.'),
  TOO_MANY: (n, roleId, partial) => errorEmbed(
    'Use a Dedicated Role',
    `<@&${roleId}> is held by **${formatNumber(n)}** members who are not live${partial}. They would all lose it. Create a dedicated one with \`/liverole set new_role:Live\`.`,
  ),
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The setting could not be saved. Please try again in a moment.'),
  CANNOT_POST: (reason) => errorEmbed('Cannot Post There', `${reason.replace(/\.?$/, '.')} Pick another channel or fix my permissions there.`),
  EMPTY_MESSAGE: () => errorEmbed('Empty Message', 'That text renders as an empty message.'),
  PING_EVERYONE: () => errorEmbed('Invalid Role', "@everyone can't be the ping role. Use a role members can opt into, such as @Stream Alerts."),
  PING_NO_CHANNEL: () => errorEmbed('No Channel Set', 'Pick the channel in the same command: `/liverole announce channel:#live-now ping_role:@Role`.'),
  PING_NOT_ALLOWED: (roleId, channelId) => errorEmbed(
    'Role Not Pingable',
    `<@&${roleId}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#${channelId}>.`,
  ),
  NO_PRESENCES: () => errorEmbed(
    'Live Role Unavailable',
    'This bot runs without presence data (`DISABLE_PRESENCES=1`), so it cannot see who is streaming. '
      + 'Ask the bot owner to turn presences back on. `/liverole disable` still removes the role from members.',
  ),
};

const OFF_NOTE = 'ℹ️ The live role is **off**. Turn it on with `/liverole enable`.';

/** Final answer: overwrites the deferred reply (which is also the confirm message when one was shown). Never pings. */
async function finish(interaction, payload) {
  const p = { ...payload, components: [], allowedMentions: { parse: [] } };
  try {
    return await interaction.editReply(p);
  } catch (err) {
    if (isInteractionGone(err)) return null;
    return safeReply(interaction, p);
  }
}

/** Loads settings, or answers "Settings Unavailable" and returns null. */
async function loadSettingsOrFinish(interaction) {
  const settings = await getSettingsSafe(interaction.guildId);
  if (!settings) await finish(interaction, { embeds: [MSG.SETTINGS_UNAVAILABLE()] });
  return settings;
}

/** Writes settings through settingsService; answers "Could Not Save" and returns null on failure. Without `field` the whole change is logged. */
async function save(interaction, sub, data, field, oldValue) {
  const guildId = interaction.guildId;
  try {
    const row = await updateSettings(guildId, data);
    const change = field ? `${field} ${JSON.stringify(oldValue ?? null)} → ${JSON.stringify(data[field] ?? null)}` : JSON.stringify(data);
    logger.info(`[liverole] ${guildId}: ${change} by ${interaction.user.id}`);
    return row;
  } catch (err) {
    logger.error(`[liverole] ${sub} save failed`, err);
    await finish(interaction, { embeds: [MSG.COULD_NOT_SAVE()] });
    return null;
  }
}

/** Runs a sync and waits for it, but not forever: null means it is still running (syncGuild never rejects). */
function boundedSync(guild, opts) {
  return withTimeout(syncGuild(guild, opts), LIVEROLE.COMMAND_SYNC_WAIT_MS, 'liverole command sync').catch(() => null);
}

/** The guild's own role object for a role option; null when there is none or the guild does not have it cached. */
function resolveRole(interaction, required) {
  const opt = interaction.options.getRole('role', required);
  return opt ? (interaction.guild.roles.cache.get(opt.id) ?? null) : null;
}

const describeRule = (roleId, requiredId) => `Members get <@&${roleId}> while Discord shows them as **Streaming** (Twitch or YouTube)`
  + (requiredId ? ` and they have <@&${requiredId}>.` : '.');

// ── /liverole set ─────────────────────────────────────────────────────────────

async function runSet(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  if (!(await resolveNewOptions(interaction, data, { required: ['role'] }))) return;
  const { guild } = interaction;
  const role = resolveRole(interaction, true);

  if (!role) return finish(interaction, { embeds: [MSG.CANNOT_USE('Role not found.')] });
  if (role.id === guild.id) return finish(interaction, { embeds: [MSG.EVERYONE()] });
  if (role.id === settings.liveRoleRequiredRoleId) return finish(interaction, { embeds: [MSG.SAME_AS_REQUIRED()] });
  // Anyone who can show a Streaming status gets this role, so it must never carry Administrator, Manage Roles or similar.
  const check = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
  if (!check.ok) return finish(interaction, { embeds: [MSG.CANNOT_USE(check.reason)] });

  if (role.id === settings.liveRoleId) {
    return finish(interaction, {
      embeds: [infoEmbed('No Change', `<@&${role.id}> is already the live role. Use \`/liverole enable\` to re-sync it.`)],
    });
  }

  // Dedicated-role guard: the sweep strips the role from every holder who is not streaming, so a populated role would be emptied.
  const complete = await loadMembers(guild);
  const n = countLosingHolders(guild, role, settings);
  if (n > LIVEROLE.SET_MAX_STRIP) {
    return finish(interaction, { embeds: [MSG.TOO_MANY(n, role.id, complete ? '' : ' (counting cached members only)')] });
  }
  if (n > 0) {
    const ok = await awaitConfirm(interaction, {
      title: 'Use this role as the live role?',
      description: `**${formatNumber(n)}** member(s) have <@&${role.id}> but are not live`
        + `${settings.liveRoleRequiredRoleId ? ' or lack the required role' : ''}. `
        + 'While the live role is on, they will lose it. A dedicated role (for example a new @Live role) is recommended.',
      confirmLabel: 'Use this role', cancelLabel: 'Cancel', danger: true,
    });
    if (!ok) return; // awaitConfirm already showed Cancelled / Timed out; nothing was written
  }

  const previousId = settings.liveRoleId;
  const row = await save(interaction, 'set', { liveRoleId: role.id }, 'liveRoleId', previousId);
  if (!row) return;

  const enabled = row.liveRoleEnabled === true;
  const result = enabled ? await boundedSync(guild, { mode: 'sync', reason: 'set' }) : undefined;

  // The previous role is never touched: the bot no longer manages it, and stripping it could hit members who got it for other reasons.
  const oldHolders = previousId && previousId !== role.id
    ? (guild.roles.cache.get(previousId)?.members.filter((m) => !m.user.bot).size ?? 0)
    : 0;

  const lines = [describeRule(role.id, settings.liveRoleRequiredRoleId)];
  if (enabled) lines.push(formatSyncResult(result));
  else lines.push(OFF_NOTE + (n > 0 ? ` Members who have <@&${role.id}> but are not live will lose it then.` : ''));
  if (oldHolders > 0) {
    lines.push(`ℹ️ **${formatNumber(oldHolders)}** member(s) still have the previous live role <@&${previousId}>. I no longer manage it, so remove it from them or delete that role.`);
  }
  return finish(interaction, { embeds: [successEmbed('Live Role Set', lines.join('\n'))] });
}

// ── /liverole require ─────────────────────────────────────────────────────────

async function runRequire(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;
  const role = resolveRole(interaction, false);
  const requiredId = (!role || role.id === guild.id) ? null : role.id;

  if (requiredId && requiredId === settings.liveRoleId) return finish(interaction, { embeds: [MSG.REQUIRED_IS_LIVE()] });
  if (requiredId === (settings.liveRoleRequiredRoleId ?? null)) {
    return finish(interaction, {
      embeds: [infoEmbed('No Change', requiredId ? `<@&${requiredId}> is already required.` : 'Everyone who streams already qualifies.')],
    });
  }

  // No checkAssignableRole here: the bot never assigns the required role, so managed roles and roles above the bot are valid.
  const row = await save(interaction, 'require', { liveRoleRequiredRoleId: requiredId }, 'liveRoleRequiredRoleId', settings.liveRoleRequiredRoleId);
  if (!row) return;

  const sync = row.liveRoleEnabled && row.liveRoleId;
  const result = sync ? await boundedSync(guild, { mode: 'sync', reason: 'require' }) : undefined;
  const head = requiredId ? `Only members with <@&${requiredId}> get the live role.` : 'Everyone who streams can get the live role.';
  return finish(interaction, {
    embeds: [successEmbed('Live Role Requirement Updated', `${head}\n${sync ? formatSyncResult(result) : OFF_NOTE}`)],
  });
}

// ── /liverole enable ──────────────────────────────────────────────────────────

async function runEnable(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;

  if (!settings.liveRoleId) return finish(interaction, { embeds: [MSG.NO_ROLE()] });
  const role = guild.roles.cache.get(settings.liveRoleId);
  if (!role) return finish(interaction, { embeds: [MSG.ROLE_DELETED()] });
  const check = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
  if (!check.ok) return finish(interaction, { embeds: [MSG.CANNOT_USE(check.reason)] });

  const wasOn = settings.liveRoleEnabled === true;
  if (!wasOn) {
    // Stale holders can exist after /module disable liverole or "disable remove_roles:false". No hard refusal here: the role was
    // accepted by `set`, and refusing could leave an admin unable to turn the feature back on.
    await loadMembers(guild);
    const n = countLosingHolders(guild, role, settings);
    if (n > 0) {
      const ok = await awaitConfirm(interaction, {
        title: 'Turn the live role on?',
        description: `**${formatNumber(n)}** member(s) have <@&${role.id}> but are not live. Turning the live role on removes it from them.`,
        confirmLabel: 'Turn on', cancelLabel: 'Cancel', danger: true,
      });
      if (!ok) return;
    }
    const row = await save(interaction, 'enable', { [MODULES.liverole.flag]: true }, MODULES.liverole.flag, false);
    if (!row) return;
  }

  const result = await boundedSync(guild, { mode: 'sync', reason: wasOn ? 'resync' : 'enable' });
  const lines = [describeRule(role.id, settings.liveRoleRequiredRoleId), formatSyncResult(result)];
  const reqId = settings.liveRoleRequiredRoleId;
  if (reqId && !guild.roles.cache.has(reqId)) {
    lines.push('⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.');
  }
  return finish(interaction, {
    embeds: [successEmbed(wasOn ? 'Live Role Already On — Resynced' : 'Live Role Enabled', lines.join('\n'))],
  });
}

// ── /liverole disable ─────────────────────────────────────────────────────────

async function runDisable(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;
  const removeRoles = interaction.options.getBoolean('remove_roles') ?? true;

  const wasOn = settings.liveRoleEnabled === true;
  if (wasOn) {
    const row = await save(interaction, 'disable', { [MODULES.liverole.flag]: false }, MODULES.liverole.flag, true);
    if (!row) return;
  }

  const clear = removeRoles && !!settings.liveRoleId;
  // Clear mode removes the role from every non-bot holder, including members who are streaming right now.
  const result = clear ? await boundedSync(guild, { mode: 'clear', reason: 'disable' }) : undefined;

  const lines = ['Nobody will get the live role when they go live. The live role and the required role stay saved.'];
  if (clear) lines.push(formatSyncResult(result));
  if (!removeRoles && settings.liveRoleId) {
    lines.push(`Members who have <@&${settings.liveRoleId}> now keep it. Run \`/liverole disable\` again to remove it from them.`);
  }
  return finish(interaction, {
    embeds: [successEmbed(wasOn ? 'Live Role Disabled' : 'Live Role Already Off', lines.join('\n'))],
  });
}

// ── /liverole announce ────────────────────────────────────────────────────────

/** Stand-in stream for the preview. */
const SAMPLE_STREAM = Object.freeze({ url: 'https://twitch.tv/example', platform: 'Twitch', title: 'Ranked grind', game: 'Valorant' });

async function runAnnounce(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;
  const channelOpt = interaction.options.getChannel('channel');
  const text = interaction.options.getString('message')?.trim() || null;
  const pingOpt = interaction.options.getRole('ping_role');
  const clearPing = interaction.options.getBoolean('clear_ping_role') === true;

  // Every option empty: stop posting. The message and ping role stay saved for next time.
  if (!channelOpt && !text && !pingOpt && !clearPing) {
    if (!settings.liveRoleAnnounceChannelId) {
      return finish(interaction, {
        embeds: [infoEmbed('No Change', 'Go-live posts are already off. Turn them on with `/liverole announce channel:#live-now`.')],
      });
    }
    const row = await save(interaction, 'announce', { liveRoleAnnounceChannelId: null }, 'liveRoleAnnounceChannelId', settings.liveRoleAnnounceChannelId);
    if (!row) return;
    return finish(interaction, {
      embeds: [successEmbed('Go-Live Posts Off', 'Nothing is posted when members go live. The live role itself keeps working.')],
    });
  }

  const data = {};
  const notes = [];
  let channel = settings.liveRoleAnnounceChannelId ? (guild.channels.cache.get(settings.liveRoleAnnounceChannelId) ?? null) : null;
  if (channelOpt) {
    channel = guild.channels.cache.get(channelOpt.id) ?? null;
    const check = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
    if (!check.ok) return finish(interaction, { embeds: [MSG.CANNOT_POST(check.reason)] });
    data.liveRoleAnnounceChannelId = channel.id;
  }
  // A channel is still saved when it lacks Embed Links: the post works, only Discord's stream preview is missing.
  if (channel && !checkChannelUsable(channel, { needs: ['EmbedLinks'] }).ok) {
    notes.push(`⚠️ I lack **Embed Links** in <#${channel.id}>, so posts show no stream preview.`);
  }

  let pingId = settings.liveRoleAnnouncePingRoleId ?? null;
  if (pingOpt) {
    if (!channel) return finish(interaction, { embeds: [MSG.PING_NO_CHANNEL()] });
    const role = guild.roles.cache.get(pingOpt.id) ?? null;
    if (!role) return finish(interaction, { embeds: [MSG.CANNOT_USE('Role not found.')] });
    if (role.id === guild.id) return finish(interaction, { embeds: [MSG.PING_EVERYONE()] });
    // The admin may only set up pings they could send themselves.
    if (pingableRoleIds(channel, interaction.member, [role.id]).refused.length) {
      return finish(interaction, { embeds: [MSG.PING_NOT_ALLOWED(role.id, channel.id)] });
    }
    pingId = role.id;
    data.liveRoleAnnouncePingRoleId = role.id;
  } else if (clearPing) {
    pingId = null;
    data.liveRoleAnnouncePingRoleId = null;
  }
  const pingRole = pingId ? (guild.roles.cache.get(pingId) ?? null) : null;
  if (pingId && !pingRole) notes.push('⚠️ The ping role was deleted, so nobody is pinged. Pick a new one with `ping_role:`.');
  if (pingRole && channel && !pingableRoleIds(channel, guild.members.me ?? null, [pingRole.id]).allowed.length) {
    notes.push(`⚠️ <@&${pingRole.id}> is not mentionable and I lack **Mention @everyone, @here and All Roles** in <#${channel.id}>, so it won't actually ping.`);
  }

  let template = settings.liveRoleAnnounceMessage?.trim() || null;
  if (text) {
    template = text.toLowerCase() === 'default' ? null : text;
    data.liveRoleAnnounceMessage = template;
  }
  const d = renderPlaceholdersDetailed(template ?? DEFAULT_LIVE_ANNOUNCE_MESSAGE, {
    member: interaction.member, user: interaction.user, guild, channel: channel ?? interaction.channel, vars: SAMPLE_STREAM,
  }, { maxLength: 2000 });
  if (!d.text.trim()) return finish(interaction, { embeds: [MSG.EMPTY_MESSAGE()] });
  if (d.unknownTokens.length) {
    notes.push(`⚠️ Unknown variables (sent as typed): ${d.unknownTokens.slice(0, 10).map((t) => `\`${t}\``).join(', ')}`);
  }
  if (template && !/\{url\}/i.test(template)) notes.push('Tip: add `{url}` to link the stream.');

  const row = await save(interaction, 'announce', data);
  if (!row) return;

  const roleText = row.liveRoleId ? `<@&${row.liveRoleId}>` : 'the live role';
  let head;
  if (channel) {
    head = `When a member goes live and gets ${roleText}, I post in <#${channel.id}>${pingRole ? ` and ping <@&${pingRole.id}>` : ''}`
      + ' (at most once an hour per member).';
  }
  else if (row.liveRoleAnnounceChannelId) head = '⚠️ The go-live channel was deleted. Pick a new one with `/liverole announce channel:`.';
  else head = 'ℹ️ No channel is set, so nothing is posted yet. Add one with `/liverole announce channel:#live-now`.';
  const lines = [head];
  if (row.liveRoleEnabled !== true) lines.push(OFF_NOTE);
  lines.push(...notes, '', '**Preview**', pingRole ? `<@&${pingRole.id}> ${d.text}` : d.text);
  return finish(interaction, {
    embeds: [successEmbed('Go-Live Posts Updated', clampText(lines.join('\n'), 4096))],
  });
}

// ── /liverole status ──────────────────────────────────────────────────────────

function announceValue(guild, settings) {
  const id = settings.liveRoleAnnounceChannelId;
  if (!id) return 'Off — `/liverole announce`';
  if (!guild.channels.cache.has(id)) return `⚠️ Channel deleted (\`${id}\`) — \`/liverole announce\``;
  const pingId = settings.liveRoleAnnouncePingRoleId;
  let ping = 'no ping';
  if (pingId) ping = guild.roles.cache.has(pingId) ? `pings <@&${pingId}>` : `⚠️ ping role deleted (\`${pingId}\`)`;
  return `<#${id}> · ${settings.liveRoleAnnounceMessage?.trim() ? 'custom message' : 'default message'} · ${ping}`;
}

async function runStatus(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;

  // Caches only (no member fetch), so status is always fast.
  const role = settings.liveRoleId ? (guild.roles.cache.get(settings.liveRoleId) ?? null) : null;
  const manage = role ? checkAssignableRole(guild, role, { allowDangerous: true }) : null;
  const dangerous = manage?.dangerous ?? [];
  const reqId = settings.liveRoleRequiredRoleId;
  const reqExists = !reqId || guild.roles.cache.has(reqId);
  const holders = role ? [...role.members.filter((m) => !m.user.bot).values()] : [];
  const streamers = [...guild.presences.cache.values()].filter((p) => isStreaming(p) && !p.member?.user?.bot);
  const enabled = settings.liveRoleEnabled === true;

  const embed = guildEmbed(
    settings,
    '🔴 Live Role',
    'Gives a role to members while Discord shows them as **Streaming**, which happens when a Twitch or YouTube account they connected goes live.',
    'Counts use cached members · MEE6: Twitch Alerts › live-streaming role',
  );

  let liveNow = '—';
  if (role) {
    const lines = [`**${formatNumber(holders.length)}** member(s) have the live role · **${formatNumber(streamers.length)}** member(s) are streaming`];
    for (const m of holders.slice(0, LIVEROLE.STATUS_LIST_MAX)) {
      lines.push(`<@${m.id}> · ${streamLabel(guild.presences.cache.get(m.id)) ?? 'not streaming'}`);
    }
    liveNow = joinWithLimit(lines, { sep: '\n', max: 1024 });
  }

  embed.addFields(
    { name: 'Status', value: enabled ? '✅ On' : '❌ Off — `/liverole enable`', inline: true },
    {
      name: 'Live role',
      value: !settings.liveRoleId ? 'Not set — `/liverole set`' : (role ? `<@&${role.id}>` : `⚠️ Deleted (\`${settings.liveRoleId}\`)`),
      inline: true,
    },
    {
      name: 'Required role',
      value: !reqId ? 'None — everyone who streams' : (reqExists ? `<@&${reqId}>` : `⚠️ Deleted (\`${reqId}\`)`),
      inline: true,
    },
    { name: 'Go-live posts', value: announceValue(guild, settings) },
    { name: 'Live now', value: liveNow },
  );

  const problems = liveRoleProblems({
    enabled,
    liveRoleId: settings.liveRoleId,
    roleExists: !!role,
    manage,
    dangerousNames: permissionNames(dangerous),
    requiredRoleId: reqId,
    requiredRoleExists: reqExists,
    holderCount: holders.length,
  });
  if (problems.length) embed.addFields({ name: 'Problems', value: joinWithLimit(problems, { sep: '\n', max: 1024 }) });

  return finish(interaction, { embeds: [embed] });
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async function runUnavailable(interaction) {
  if (!(await safeDefer(interaction))) return;
  return finish(interaction, { embeds: [MSG.NO_PRESENCES()] });
}

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  // Without presences every member looks offline; only the cleanup (disable) still makes sense.
  if (sub !== 'disable' && !presencesEnabled()) return runUnavailable(interaction);
  if (sub === 'set') return runSet(interaction);
  if (sub === 'require') return runRequire(interaction);
  if (sub === 'enable') return runEnable(interaction);
  if (sub === 'disable') return runDisable(interaction);
  if (sub === 'announce') return runAnnounce(interaction);
  if (sub === 'status') return runStatus(interaction);
}
