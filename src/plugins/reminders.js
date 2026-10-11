// Reminders plugin (pkg-reminders): the channelDelete cleanup, the guildReady self-healing pass, the persistent
// `reminder-modal` form handler and the `reminder.fire` job handler. The logic lives in services/reminderService.js.
import prisma from '../database/client.js';
import { PERMISSION_LEVELS } from '../config/constants.js';
import { parseCustomId } from '../config/customIds.js';
import { getSettingsSafe, peekSettings } from '../services/settingsService.js';
import {
  DEFAULT_TEMPLATE,
  REMINDER_CHANNEL_TYPES,
  computeNextRun,
  disableRemindersForChannel,
  fireReminder,
  isEmptyTemplate,
  reconcileGuildReminders,
  reminderWarnings,
  resolveTimeZone,
  summaryLines,
  syncReminderJob,
} from '../services/reminderService.js';
import { TimeoutError, ackBudgetMs, withTimeout } from '../utils/async.js';
import { normalizeTemplate, parseTemplateModal, validateTemplate } from '../utils/customEmbed.js';
import { successEmbed } from '../utils/embedBuilder.js';
import { checkChannelUsable, replyError } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { clampText, joinWithLimit } from '../utils/text.js';

/**
 * Submit of the message editor opened by /reminder create and /reminder edit. The customId is
 * `reminder-modal:{reminderId}` and the inputs are the foundation `tpl-*` fields. The component is registered with
 * `module: null` (the command is manageable while the module is off), so permission is re-checked here.
 * @param {import('discord.js').ModalSubmitInteraction} interaction
 */
export async function handleReminderModal(interaction) {
  const { params: [reminderId] } = parseCustomId(interaction.customId);
  if (!interaction.inCachedGuild() || !reminderId) {
    return replyError(interaction, 'Form Expired', 'This form has expired. Please run the command again.');
  }
  const guildId = interaction.guildId;

  // Everything before the acknowledgement is bounded (00-overview rule 21).
  let settings = peekSettings(guildId);
  if (!settings) {
    settings = await withTimeout(getSettingsSafe(guildId), ackBudgetMs(interaction), 'reminder modal settings').catch(() => null);
  }

  // The router checks levels for commands, not for components.
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN) {
    return replyError(interaction, 'Insufficient Permissions', 'You need the Admin level to edit reminders.');
  }

  let r;
  try {
    // Guild-scoped lookup: an id from another guild is simply not found.
    r = await withTimeout(prisma.reminder.findFirst({ where: { id: reminderId, guildId } }), ackBudgetMs(interaction), 'reminder modal lookup');
  } catch (err) {
    if (err instanceof TimeoutError) return replyError(interaction, 'Temporarily Unavailable', 'That took too long. Please try again.');
    throw err;
  }
  if (!r) return replyError(interaction, 'Reminder Not Found', 'This reminder no longer exists.');

  // Most parse errors are repeated by the validation below, and an empty form means "the default text". An unreadable
  // colour is the exception: the parser keeps the old colour, so nothing later would tell the admin.
  const { template, errors } = parseTemplateModal(interaction, normalizeTemplate(r.template));
  const colourError = errors.find((e) => e.startsWith('Colour must'));
  if (colourError) return replyError(interaction, 'Invalid Message', `${colourError}
Nothing was saved.`);
  const tpl = isEmptyTemplate(template) ? { ...DEFAULT_TEMPLATE, buttons: [] } : normalizeTemplate(template);

  const v = validateTemplate(tpl, { requireSomething: true });
  if (!v.ok) {
    return replyError(interaction, 'Invalid Message', `${joinWithLimit(v.errors, { sep: '\n', max: 1000 })}\nNothing was saved.`);
  }

  if (tpl.embed) {
    const channel = interaction.guild.channels.cache.get(r.channelId);
    if (channel && !checkChannelUsable(channel, { needs: ['EmbedLinks'] }).ok) {
      return replyError(interaction, 'Missing Permissions', `I need **Embed Links** in <#${r.channelId}> to post an embed. Nothing was saved.`);
    }
  }

  await interaction.deferReply();
  settings ??= await getSettingsSafe(guildId);

  // The form never changes `enabled` or the schedule.
  const updated = await prisma.reminder.update({ where: { id: r.id }, data: { template: tpl } });

  const tz = resolveTimeZone(settings?.timezone);
  let next = null;
  if (updated.enabled) {
    try {
      next = await syncReminderJob(updated, settings); // idempotent re-sync
    } catch (err) {
      logger.error(`reminder modal: scheduling ${updated.id} failed`, err);
      next = computeNextRun(updated, tz, new Date());
    }
  }

  const lines = summaryLines(updated, settings, next, reminderWarnings(updated, interaction.guild, tpl));
  await interaction.editReply({ embeds: [successEmbed('Reminder Saved', clampText(lines.join('\n'), 4096))] });
}

const hooks = [
  {
    event: 'channelDelete',
    id: 'reminders', // HOOK_ORDER.channelDelete lists 'reminders'
    always: true, // cleanup must run even while the module is off
    filter: (ctx) => REMINDER_CHANNEL_TYPES.includes(ctx.channel?.type),
    run: (ctx) => disableRemindersForChannel(ctx.guildId, ctx.channel.id),
  },
  {
    // Self-healing: an enabled reminder without a live job (guild left and rejoined, a lost job row) gets it back.
    event: 'guildReady',
    id: 'reminders',
    always: true,
    timeoutMs: 30_000,
    run: (ctx) => reconcileGuildReminders(ctx.guild),
  },
];

export const plugin = {
  id: 'reminders',
  module: 'reminders',
  hooks,
  components: [
    // module: null so the editor works while the module is off (the command is `gated = false`).
    { type: 'modal', prefix: 'reminder-modal', module: null, handler: handleReminderModal },
  ],
  jobs: [{ type: 'reminder.fire', handler: fireReminder }],
  provides: {},
};
