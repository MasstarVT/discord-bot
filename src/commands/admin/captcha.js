import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import {
  CUSTOM_IDS, CaptchaConfigError, DEFAULT_ROLE_NAME, PANEL_NEEDS, STYLE_NAMES, applyLock, auditReason, botSeesWithoutEveryone,
  buildStatusEmbed, countPending, createVerificationChannel, diagnoseCaptcha, discordErrorText, everyoneCanView,
  getBulkRun, isImageCaptchaAvailable, isLocked, lockPlan, panelTemplate, parseVerifyButton, progressText, publishPanel,
  refreshPanel, removeLock, resetPanelTemplate, settingsForAck, startVerifyExisting, validateTimeoutMinutes,
  checkVerifiedRole, MSG,
} from '../../services/captchaService.js';
import { getSettings, updateSettings, updateSettingsWith } from '../../services/settingsService.js';
import { cancelJobs } from '../../services/scheduler.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { errorEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  awaitConfirm, checkAssignableRole, checkChannelUsable, missingBotPermissions, permissionNames, safeReply,
} from '../../utils/interactionHelpers.js';
import { buildTemplateModal } from '../../utils/customEmbed.js';
import { clampText, formatNumber } from '../../utils/text.js';
import { formatDurationLong } from '../../utils/parseDuration.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';

const TXT = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const MODES = [{ name: 'Image code', value: 'IMAGE' }, { name: 'Button click', value: 'BUTTON' }];

export const module = 'captcha';
export const gated = false; // configuration works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('captcha')
  .setDescription('Captcha verification for new members (MEE6 Welcome › Captcha verification).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('setup').setDescription('Create or reuse #verification and the role, then post the panel.')
    .addChannelOption((o) => o.setName('channel').setDescription('Verification channel (default: create one)').addChannelTypes(...TXT))
    .addStringOption((o) => newChannelOption(o))
    .addRoleOption((o) => o.setName('role').setDescription('Verified role (default: create one)'))
    .addStringOption((o) => newRoleOption(o))
    .addStringOption((o) => o.setName('mode').setDescription('How members verify').addChoices(...MODES)))
  .addSubcommand((s) => s.setName('enable').setDescription('Turn verification on.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn verification off (settings are kept).'))
  .addSubcommand((s) => s.setName('status').setDescription('Show the settings and any problems.'))
  .addSubcommand((s) => s.setName('mode').setDescription('Image code or one-click button.')
    .addStringOption((o) => o.setName('type').setDescription('How members verify').setRequired(true).addChoices(...MODES)))
  .addSubcommand((s) => s.setName('role').setDescription('Set the verified role.')
    .addRoleOption((o) => o.setName('role').setDescription('Verified role'))
    .addStringOption((o) => newRoleOption(o)))
  .addSubcommand((s) => s.setName('channel').setDescription('Move the panel to another channel.')
    .addChannelOption((o) => o.setName('channel').setDescription('Verification channel').addChannelTypes(...TXT))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('timeout').setDescription('Kick members who do not verify in time.')
    .addIntegerOption((o) => o.setName('minutes').setDescription('Minutes after joining, 5-10080; 0 = never').setRequired(true).setMinValue(0).setMaxValue(10080)))
  .addSubcommand((s) => s.setName('message').setDescription('Edit the panel (no options opens a form).')
    .addStringOption((o) => o.setName('button_label').setDescription('Verify button text').setMaxLength(80))
    .addStringOption((o) => o.setName('button_color').setDescription('Verify button colour')
      .addChoices({ name: 'Green', value: 'SUCCESS' }, { name: 'Blurple', value: 'PRIMARY' }, { name: 'Grey', value: 'SECONDARY' }, { name: 'Red', value: 'DANGER' }))
    .addBooleanOption((o) => o.setName('reset').setDescription('Restore the default panel')))
  .addSubcommand((s) => s.setName('lock').setDescription('Hide channels until members verify (MEE6 deny View Channels).')
    .addStringOption((o) => o.setName('state').setDescription('on or off').setRequired(true)
      .addChoices({ name: 'on', value: 'on' }, { name: 'off', value: 'off' })))
  .addSubcommand((s) => s.setName('verify-existing').setDescription('Give the verified role to current members.')
    .addBooleanOption((o) => o.setName('include_pending').setDescription('Also verify members still waiting'))
    .addBooleanOption((o) => o.setName('stop').setDescription('Stop a running batch')));

const LOCK_TIP = '• Unverified members can still see every channel @everyone can see. Hide them with `/captcha lock state:on`.';

const hasAnyOption = (i) => i.options.getString('button_label') !== null
  || i.options.getString('button_color') !== null
  || i.options.getBoolean('reset') !== null;

const unavailable = () => errorEmbed('Temporarily Unavailable', 'Settings could not be loaded. Try again in a moment.');

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  try {
    if (sub === 'message' && !hasAnyOption(interaction)) return await openPanelModal(interaction); // showModal is the first response
    if (sub === 'lock') return await runLock(interaction); // awaitConfirm replies first
    await interaction.deferReply(); // public (admin convention)
    if (!(await resolveNewOptions(interaction, data, sub === 'setup' ? undefined : { required: ['role', 'channel'] }))) return undefined;
    const s = await getSettings(interaction.guildId); // a memo hit: the router loaded it
    switch (sub) {
      case 'setup': return await doSetup(interaction, s);
      case 'enable': return await doEnable(interaction);
      case 'disable': return await doDisable(interaction);
      case 'status': return await interaction.editReply({ embeds: [await buildStatusEmbed(interaction.guild, s)] });
      case 'mode': return await doMode(interaction);
      case 'role': return await doRole(interaction, s);
      case 'channel': return await doChannel(interaction, s);
      case 'timeout': return await doTimeout(interaction);
      case 'message': return await doMessage(interaction);
      case 'verify-existing': return await doVerifyExisting(interaction, s);
      default: return await interaction.editReply({ embeds: [errorEmbed('Unknown Subcommand', 'That subcommand does not exist.')] });
    }
  } catch (err) {
    if (err instanceof CaptchaConfigError) {
      return safeReply(interaction, { embeds: [errorEmbed(err.title ?? 'Not Saved', err.message)] });
    }
    throw err; // the router's generic handler
  }
}

// ── setup ─────────────────────────────────────────────────────────────────────

const CHANNEL_CREATE_NEEDS = [
  'ManageChannels', 'ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory',
  'AddReactions', 'CreatePublicThreads', 'CreatePrivateThreads',
];

async function doSetup(interaction, s) {
  const { guild, guildId } = interaction;
  const roleOpt = interaction.options.getRole('role');
  const channelOpt = interaction.options.getChannel('channel');
  const mode = interaction.options.getString('mode');

  const storedChannel = s.captchaChannelId ? guild.channels.cache.get(s.captchaChannelId) : null;
  const storedUsable = !!storedChannel && checkChannelUsable(storedChannel, { needs: PANEL_NEEDS }).ok;
  const willCreateChannel = !channelOpt && !storedUsable;
  const missing = missingBotPermissions(guild, ['ManageRoles', ...(willCreateChannel ? CHANNEL_CREATE_NEEDS : [])]);
  if (missing.length) {
    throw new CaptchaConfigError(`I need **${permissionNames(missing).join(', ')}** in this server to set up verification.`, 'Missing Permissions');
  }

  const created = [];

  // Role: the option, else the stored one, else a new `verified` role with no permissions.
  let role;
  if (roleOpt) {
    const chk = checkVerifiedRole(guild, roleOpt, s, { executor: interaction.member });
    if (!chk.ok) throw new CaptchaConfigError(chk.reason, 'Cannot Use This Role');
    role = roleOpt;
  } else if (s.captchaRoleId && guild.roles.cache.has(s.captchaRoleId)) {
    role = guild.roles.cache.get(s.captchaRoleId);
    const chk = checkVerifiedRole(guild, role, s);
    if (!chk.ok) throw new CaptchaConfigError(`${chk.reason} Pick another role with the \`role\` option.`, 'Cannot Use This Role');
  } else {
    role = await guild.roles.create({
      name: DEFAULT_ROLE_NAME, permissions: [], hoist: false, mentionable: false, reason: auditReason(interaction, 'captcha setup'),
    });
    created.push(`role <@&${role.id}>`);
  }

  // Channel: the option, else the stored usable one, else a new #verification.
  let channel;
  if (channelOpt) {
    const chk = checkChannelUsable(channelOpt, { needs: PANEL_NEEDS });
    if (!chk.ok) throw new CaptchaConfigError(chk.reason, 'Cannot Use That Channel');
    channel = channelOpt;
  } else if (storedUsable) {
    channel = storedChannel;
  } else {
    try {
      channel = await createVerificationChannel(guild, interaction);
      created.push(`channel <#${channel.id}>`);
    } catch (err) {
      const text = discordErrorText(err);
      const lead = created.length ? `I created ${created.join(' and ')} but could not create the channel` : 'I could not create the channel';
      return interaction.editReply({ embeds: [errorEmbed('Setup Incomplete', `${lead}: ${text}`)] });
    }
  }

  const next = { ...s, captchaRoleId: role.id, captchaChannelId: channel.id, ...(mode && { captchaMode: mode }) };
  let msg;
  try {
    msg = await publishPanel(guild, channel, next, { channelId: s.captchaChannelId, messageId: s.captchaMessageId });
  } catch (err) {
    if (err instanceof CaptchaConfigError && created.length) {
      throw new CaptchaConfigError(`${err.message}\n\nI created ${created.join(' and ')} before this failed; nothing was saved.`, err.title);
    }
    throw err;
  }

  const after = await updateSettings(guildId, {
    captchaEnabled: true,
    captchaRoleId: role.id,
    captchaChannelId: channel.id,
    captchaMessageId: msg.id,
    ...(mode && { captchaMode: mode }),
  });

  const lines = [
    `**Channel:** <#${channel.id}> ([panel](${msg.url}))`,
    `**Role:** <@&${role.id}>`,
    `**Mode:** ${after.captchaMode === 'BUTTON' ? 'Button click' : 'Image code'}`,
  ];
  if (created.length) lines.push(`**Created:** ${created.join(', ')}`);
  lines.push('', `• New members must verify in <#${channel.id}> before they get <@&${role.id}> and their autoroles.`);
  if (!isLocked(guild)) lines.push(LOCK_TIP);
  lines.push(`• Give <@&${role.id}> to members who are already here with \`/captcha verify-existing\`.`);
  if (after.captchaTimeoutMinutes === 0) lines.push('• Optionally kick members who never verify: `/captcha timeout`.');
  for (const p of await diagnoseCaptcha(guild, after, { checkPanel: false })) lines.push(`⚠️ ${p}`);
  return interaction.editReply({ embeds: [successEmbed('Verification Ready', lines.join('\n'))] });
}

// ── enable / disable ──────────────────────────────────────────────────────────

async function doEnable(interaction) {
  const { guild, guildId } = interaction;
  const after = await updateSettings(guildId, { captchaEnabled: true });
  const problems = await diagnoseCaptcha(guild, after);
  const role = after.captchaRoleId ? guild.roles.cache.get(after.captchaRoleId) : null;
  let text = `New members must verify before they get ${role ? `<@&${role.id}>` : 'the verified role'} and their autoroles.`;
  if (problems.length) text += `\n\n**Fix these first:**\n${problems.map((p) => `• ${p}`).join('\n')}`;
  else if (!isLocked(guild)) text += `\n\n${LOCK_TIP}`;
  return interaction.editReply({ embeds: [successEmbed('Verification Enabled', text)] });
}

async function doDisable(interaction) {
  const { guild, guildId } = interaction;
  const pending = await countPending(guildId);
  const after = await updateSettings(guildId, { captchaEnabled: false });
  let text = 'New members no longer need to verify. The panel, role and settings are kept.';
  if (pending > 0) {
    text += `\n\n**${formatNumber(pending)}** member(s) were still waiting to verify. They will not be kicked, but they have not received their autoroles. `
      + 'To let them in now, run `/captcha verify-existing include_pending:true`.';
  }
  const role = after.captchaRoleId ? guild.roles.cache.get(after.captchaRoleId) : null;
  if (isLocked(guild) && role) {
    text += `\n\n⚠️ The lock-down is still on: members without <@&${role.id}> only see <#${after.captchaChannelId}>. Undo it with \`/captcha lock state:off\`.`;
  }
  return interaction.editReply({ embeds: [successEmbed('Verification Disabled', text)] });
}

// ── mode / role / channel / timeout ───────────────────────────────────────────

async function doMode(interaction) {
  const type = interaction.options.getString('type', true);
  await updateSettings(interaction.guildId, { captchaMode: type });
  let text = type === 'IMAGE'
    ? 'Members click **Verify** and type the 6-character code from an image (3 tries per image; codes expire after 10 minutes).'
    : 'Members click **Verify** once. This stops accidental joins and simple bots, but not determined ones.';
  if (type === 'IMAGE' && !isImageCaptchaAvailable()) text += `\n⚠️ ${MSG.FONT_MISSING}`;
  return interaction.editReply({ embeds: [successEmbed('Verification Mode Set', text)] });
}

async function doRole(interaction, s) {
  const { guild, guildId } = interaction;
  const role = interaction.options.getRole('role', true);
  const chk = checkVerifiedRole(guild, role, s, { executor: interaction.member });
  if (!chk.ok) throw new CaptchaConfigError(chk.reason, 'Cannot Use This Role');
  if (role.id === s.captchaRoleId) {
    return interaction.editReply({ embeds: [infoEmbed('No Change', `<@&${role.id}> is already the verified role.`)] });
  }
  await updateSettings(guildId, { captchaRoleId: role.id });
  const lines = [`<@&${role.id}> is given to members after they verify.`];
  if (s.captchaRoleId && guild.roles.cache.has(s.captchaRoleId)) {
    lines.push(`Members who verified with <@&${s.captchaRoleId}> don't have this role. Run \`/captcha verify-existing\` to give it to everyone already here.`);
  }
  if ((s.autoroleIds ?? []).includes(role.id)) {
    lines.push('This role is also an autorole. Autoroles are given after verification, so members get it then.');
  }
  if (isLocked(guild) && !role.permissions.has(PermissionFlagsBits.ViewChannel)) {
    lines.push('⚠️ The server is locked: run `/captcha lock state:on` again so this role gets View Channels.');
  }
  return interaction.editReply({ embeds: [successEmbed('Verified Role Set', lines.join('\n'))] });
}

async function doChannel(interaction, s) {
  const { guild, guildId } = interaction;
  const channel = interaction.options.getChannel('channel', true);
  const chk = checkChannelUsable(channel, { needs: PANEL_NEEDS });
  if (!chk.ok) throw new CaptchaConfigError(chk.reason, 'Cannot Use That Channel');
  const msg = await publishPanel(guild, channel, s, { channelId: s.captchaChannelId, messageId: s.captchaMessageId });
  await updateSettings(guildId, { captchaChannelId: channel.id, captchaMessageId: msg.id });
  const lines = [`The verification panel is now in <#${channel.id}> ([jump](${msg.url})).`];
  if (isLocked(guild) && !everyoneCanView(channel)) {
    lines.push("⚠️ The server is locked but @everyone can't see this channel. Run `/captcha lock state:on` again to fix it.");
  }
  if (!s.captchaRoleId) lines.push('Set the verified role with `/captcha setup` or `/captcha role`.');
  return interaction.editReply({ embeds: [successEmbed('Panel Moved', lines.join('\n'))] });
}

async function doTimeout(interaction) {
  const { guild, guildId } = interaction;
  const v = validateTimeoutMinutes(interaction.options.getInteger('minutes', true));
  if (v.error) throw new CaptchaConfigError(v.error);
  await updateSettings(guildId, { captchaTimeoutMinutes: v.minutes });
  if (v.minutes === 0) {
    const n = await cancelJobs({ guildId, type: 'captcha.timeout' });
    return interaction.editReply({
      embeds: [successEmbed('Timeout Off', `Unverified members are never kicked.${n ? ` Cancelled ${n} scheduled kick(s).` : ''}`)],
    });
  }
  let text = `Members who haven't verified **${formatDurationLong(v.minutes * 60_000)}** after joining are kicked (they can rejoin). `
    + 'Members already waiting keep the deadline they got when they joined.';
  if (missingBotPermissions(guild, ['KickMembers']).length) text += '\n⚠️ I need **Kick Members** to remove unverified members.';
  return interaction.editReply({ embeds: [successEmbed('Timeout Set', text)] });
}

// ── message ───────────────────────────────────────────────────────────────────

async function openPanelModal(interaction) {
  const s = await settingsForAck(interaction); // the memo, else a lookup bounded by the acknowledgement budget
  if (!s) return interaction.reply({ embeds: [unavailable()], ephemeral: true });
  return interaction.showModal(buildTemplateModal({
    customId: CUSTOM_IDS.modalPanel, title: 'Verification panel', template: panelTemplate(s),
  }));
}

async function doMessage(interaction) {
  const { guild, guildId } = interaction;
  const label = interaction.options.getString('button_label')?.trim();
  const color = interaction.options.getString('button_color');
  const reset = interaction.options.getBoolean('reset');

  if (reset !== null && (label !== undefined || color !== null)) throw new CaptchaConfigError('Use reset on its own.');
  if (reset === true) {
    const after = await resetPanelTemplate(guildId);
    const r = await refreshPanel(guild, after);
    return interaction.editReply({
      embeds: [successEmbed('Panel Reset', `The default verification panel is back.${r.ok ? '' : `\n${r.reason}`}`)],
    });
  }
  if (reset === false && label === undefined && color === null) {
    throw new CaptchaConfigError('Choose an option, or run `/captcha message` with no options to open the form.');
  }
  if (label === '') throw new CaptchaConfigError('The button label cannot be empty.');

  const after = await updateSettingsWith(guildId, (cur) => ({
    captchaTemplate: {
      ...panelTemplate(cur),
      verifyButton: {
        ...parseVerifyButton(cur.captchaTemplate?.verifyButton),
        ...(label && { label: clampText(label, 80) }),
        ...(color && { style: color }),
      },
    },
  }));
  const r = await refreshPanel(guild, after);
  const b = parseVerifyButton(after.captchaTemplate?.verifyButton);
  return interaction.editReply({
    embeds: [successEmbed('Verify Button Updated', `Label **${b.label}**, colour **${STYLE_NAMES[b.style]}**.${r.ok ? '' : `\n${r.reason}`}`)],
  });
}

// ── lock ──────────────────────────────────────────────────────────────────────

async function runLock(interaction) {
  const state = interaction.options.getString('state', true);
  const s = await settingsForAck(interaction);
  if (!s) return interaction.reply({ embeds: [unavailable()], ephemeral: true });
  return state === 'on' ? lockOn(interaction, s) : lockOff(interaction, s);
}

async function lockOn(interaction, s) {
  const { guild, guildId } = interaction;
  const role = guild.roles.cache.get(s.captchaRoleId ?? '');
  const channel = guild.channels.cache.get(s.captchaChannelId ?? '');
  const fail = (title, text) => interaction.reply({ embeds: [errorEmbed(title, text)] });

  if (!role || !channel) {
    return fail('Set Up First', 'Run `/captcha setup` first: the lock needs a verified role and a verification channel.');
  }
  const assignable = checkAssignableRole(guild, role, { allowDangerous: false });
  if (!assignable.ok) return fail('Cannot Lock', `${assignable.reason} I need to edit <@&${role.id}>'s permissions.`);
  if (!botSeesWithoutEveryone(guild)) {
    return fail('Cannot Lock', "My own role doesn't have **View Channels**, so locking would hide the server from me too. Give my role View Channels in **Server Settings → Roles**, then try again.");
  }
  const missing = missingBotPermissions(channel, ['ViewChannel', 'ManageRoles']);
  if (missing.length) {
    const names = permissionNames(missing).map((n) => (n === 'Manage Roles' ? 'Manage Permissions' : n)).join(', ');
    return fail('Cannot Lock', `I need **${names}** in <#${channel.id}> so unverified members can still see it.`);
  }

  const plan = lockPlan(guild, role, channel);
  if (!plan.channel && !plan.role && !plan.everyone) {
    return interaction.reply({
      embeds: [infoEmbed('Already Locked', `Members without <@&${role.id}> only see <#${channel.id}> and channels that explicitly allow @everyone.`)],
    });
  }

  const lines = ['This will:'];
  if (plan.channel) lines.push(`• let @everyone see <#${channel.id}>, so new members can verify`);
  if (plan.role) lines.push(`• give <@&${role.id}> the **View Channels** permission`);
  lines.push(plan.everyone ? '• remove **View Channels** from @everyone, server-wide' : "• (@everyone already can't view channels)");
  lines.push('', `Members without <@&${role.id}> will then only see <#${channel.id}> and channels that explicitly allow @everyone. `
    + '**Give the role to members already here first** with `/captcha verify-existing`, or they lose access too.');
  if (guild.members.cache.size >= guild.memberCount) {
    const n = guild.members.cache.filter((m) => !m.roles.cache.has(role.id)).size;
    lines.push(`**${formatNumber(n)}** current member(s) don't have <@&${role.id}> yet.`);
  }
  if (getBulkRun(guildId)) lines.push('A verify-existing batch is still running.');
  if (guild.roles.everyone.permissions.has(PermissionFlagsBits.Administrator, false)) {
    lines.push('', '⚠️ @everyone has **Administrator**, so everyone still sees every channel: the lock has no effect until you remove it.');
  }

  const ok = await awaitConfirm(interaction, {
    title: '🔒 Lock the server until members verify?',
    description: lines.join('\n'),
    confirmLabel: 'Lock',
    cancelLabel: 'Cancel',
    danger: true,
    timeoutMs: 60_000,
    ephemeral: false,
  });
  if (!ok) return undefined; // awaitConfirm already showed "Cancelled" or "Timed out"

  // Remember whether @everyone could view channels before the lock, so `lock off` never opens a private server.
  const prior = guild.roles.everyone.permissions.has(PermissionFlagsBits.ViewChannel, false);
  const res = await applyLock(guild, { role, channel, plan, reason: auditReason(interaction, 'captcha lock-down') });
  let embed;
  if (res.error) {
    embed = errorEmbed('Lock Incomplete', `${res.error}\n\nDone: ${res.done.join(', ') || 'nothing'}.`);
  } else {
    try {
      await updateSettingsWith(guildId, (cur) => (cur.captchaLockdownPrior == null ? { captchaLockdownPrior: prior } : null));
    } catch (err) {
      logger.warn(`Captcha: could not store the lock-down prior state in ${guildId}`, err);
    }
    embed = successEmbed('Server Locked',
      `Members without <@&${role.id}> now only see <#${channel.id}> (and channels that explicitly allow @everyone). Undo with \`/captcha lock state:off\`.`);
  }
  return interaction.editReply({ embeds: [embed], components: [] }).catch((err) => logger.debug(`captcha lock reply: ${err?.message}`));
}

async function lockOff(interaction, s) {
  const { guild, guildId } = interaction;
  if (!isLocked(guild)) {
    if (s.captchaLockdownPrior != null) await updateSettings(guildId, { captchaLockdownPrior: null }).catch(() => {});
    return interaction.reply({ embeds: [infoEmbed('Not Locked', '@everyone can already see channels.')] });
  }
  if (missingBotPermissions(guild, ['ManageRoles']).length) {
    return interaction.reply({ embeds: [errorEmbed('Cannot Unlock', 'I need **Manage Roles** to change @everyone.')] });
  }
  if (s.captchaLockdownPrior === false) {
    await updateSettings(guildId, { captchaLockdownPrior: null });
    return interaction.reply({
      embeds: [infoEmbed('Lock-down Cleared', 'The server was private before the lock-down, so @everyone stays without View Channels.')],
    });
  }
  let description = 'This gives **View Channels** back to @everyone, so everyone can see channels whether or not they verified. Verification keeps running.';
  if (s.captchaLockdownPrior == null) {
    description += '\n\nIf @everyone was not allowed to view channels before you locked the server, do not unlock.';
  }
  const ok = await awaitConfirm(interaction, {
    title: '🔓 Unlock the server?', description, confirmLabel: 'Unlock', danger: false, timeoutMs: 60_000, ephemeral: false,
  });
  if (!ok) return undefined;
  const res = await removeLock(guild, auditReason(interaction, 'captcha unlock'));
  if (!res.error) {
    try {
      await updateSettings(guildId, { captchaLockdownPrior: null });
    } catch (err) {
      logger.warn(`Captcha: could not clear the lock-down prior state in ${guildId}`, err);
    }
  }
  const embed = res.error
    ? errorEmbed('Unlock Failed', res.error)
    : successEmbed('Server Unlocked', '@everyone can see channels again. The verified role keeps **View Channels**, and the verification channel overwrite stays.');
  return interaction.editReply({ embeds: [embed], components: [] }).catch((err) => logger.debug(`captcha unlock reply: ${err?.message}`));
}

// ── verify-existing ───────────────────────────────────────────────────────────

async function doVerifyExisting(interaction, s) {
  if (interaction.options.getBoolean('stop') === true) {
    const run = getBulkRun(interaction.guildId);
    if (!run) return interaction.editReply({ embeds: [infoEmbed('Nothing Running', 'No verify-existing batch is running.')] });
    run.abort = true;
    return interaction.editReply({ embeds: [infoEmbed('Stopping', `Stopping after the current member (${progressText(run)}).`)] });
  }
  // Edits the deferred reply itself and returns once the batch is started (it is not awaited).
  return startVerifyExisting(interaction, s, { includePending: interaction.options.getBoolean('include_pending') === true });
}
