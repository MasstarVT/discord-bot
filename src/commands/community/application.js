// /application: members apply with `apply`; admins build the forms. Thin: the rules, the apply and review flows and the DB
// access live in services/applicationService.js. Mixed command: the base level is MEMBER (so `apply` is open to everyone) and
// every other subcommand is ADMIN through subcommandPermissions. Because of that it cannot carry default_member_permissions
// (Discord would hide `apply` from members); the router enforces the level. gated = false so admins can set it up first;
// `apply` refuses while the module is off.
import { ChannelType, SlashCommandBuilder } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import {
  awaitConfirm, checkAssignableRole, checkChannelUsable, respondAutocomplete, safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { chunkLines, clampText } from '../../utils/text.js';
import { getSettingsSafe, isModuleEnabled, peekSettings } from '../../services/settingsService.js';
import {
  APPLICATION, addQuestion, beginApplication, changeRole, createForm, deleteForm, deleteMessageQuietly, findForm,
  findFormsForAutocomplete, isClearWord, listForms, parseQuestions, pendingCount, postPanel, refreshPanel, refusalEmbed,
  removeQuestion, updateForm,
} from '../../services/applicationService.js';

export const module = 'applications';
export const gated = false; // admins can set forms up while the module is off; `apply` checks the module itself
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
export const subcommandPermissions = Object.freeze({
  'form create': PERMISSION_LEVELS.ADMIN,
  'form edit': PERMISSION_LEVELS.ADMIN,
  'form delete': PERMISSION_LEVELS.ADMIN,
  'form list': PERMISSION_LEVELS.ADMIN,
  'form role': PERMISSION_LEVELS.ADMIN,
  'question add': PERMISSION_LEVELS.ADMIN,
  'question remove': PERMISSION_LEVELS.ADMIN,
  panel: PERMISSION_LEVELS.ADMIN,
});
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is 3000 characters: keep the descriptions short.

const TEXT_CHANNELS = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const formOption = (o) => o.setName('form').setDescription('The application form.').setRequired(true).setAutocomplete(true).setMaxLength(100);
const reviewOption = (o) => o.setName('review_channel').setDescription('Where answers are posted for review.').addChannelTypes(...TEXT_CHANNELS);

export const data = new SlashCommandBuilder()
  .setName('application')
  .setDescription('Apply for something here, or build application forms.')
  .addSubcommand((s) => s.setName('apply').setDescription('Fill in an application form.').addStringOption(formOption))
  .addSubcommandGroup((g) => g.setName('form').setDescription('Build application forms.')
    .addSubcommand((s) => s.setName('create').setDescription('Create a form.')
      .addStringOption((o) => o.setName('name').setDescription('The form name.').setRequired(true).setMaxLength(APPLICATION.NAME_MAX))
      .addStringOption((o) => o.setName('description').setDescription('Shown on the panel.').setMaxLength(APPLICATION.DESCRIPTION_MAX))
      .addChannelOption(reviewOption))
    .addSubcommand((s) => s.setName('edit').setDescription('Change a form. Text options take "none" to clear.')
      .addStringOption(formOption)
      .addStringOption((o) => o.setName('name').setDescription('A new name.').setMaxLength(APPLICATION.NAME_MAX))
      .addStringOption((o) => o.setName('description').setDescription('Shown on the panel.').setMaxLength(APPLICATION.DESCRIPTION_MAX))
      .addChannelOption(reviewOption)
      .addIntegerOption((o) => o.setName('cooldown_hours').setDescription('Hours before a denied member may apply again.')
        .setMinValue(0).setMaxValue(APPLICATION.COOLDOWN_MAX_HOURS))
      .addBooleanOption((o) => o.setName('enabled').setDescription('Whether members can apply.'))
      .addStringOption((o) => o.setName('accept_message').setDescription('DM on accept. Use {form} {user} {server}.').setMaxLength(APPLICATION.MESSAGE_MAX))
      .addStringOption((o) => o.setName('deny_message').setDescription('DM on deny. Use {form} {user} {server}.').setMaxLength(APPLICATION.MESSAGE_MAX)))
    .addSubcommand((s) => s.setName('delete').setDescription('Delete a form and its submissions.').addStringOption(formOption))
    .addSubcommand((s) => s.setName('list').setDescription('List the forms.'))
    .addSubcommand((s) => s.setName('role').setDescription('Roles given on accept, or needed to apply.')
      .addStringOption(formOption)
      .addStringOption((o) => o.setName('list').setDescription('Which list.').setRequired(true)
        .addChoices({ name: 'Given on accept', value: 'accept' }, { name: 'Needed to apply', value: 'required' }))
      .addRoleOption((o) => o.setName('role').setDescription('The role.').setRequired(true))
      .addBooleanOption((o) => o.setName('remove').setDescription('Remove it instead of adding it.'))))
  .addSubcommandGroup((g) => g.setName('question').setDescription('Edit the questions of a form.')
    .addSubcommand((s) => s.setName('add').setDescription('Add a question.')
      .addStringOption(formOption)
      .addStringOption((o) => o.setName('label').setDescription('The question.').setRequired(true).setMaxLength(APPLICATION.LABEL_MAX))
      .addStringOption((o) => o.setName('style').setDescription('Answer length (default short).')
        .addChoices({ name: 'Short answer', value: 'SHORT' }, { name: 'Paragraph', value: 'PARAGRAPH' }))
      .addBooleanOption((o) => o.setName('required').setDescription('Must be answered (default yes).'))
      .addStringOption((o) => o.setName('placeholder').setDescription('Hint inside the box.').setMaxLength(APPLICATION.PLACEHOLDER_MAX)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a question.')
      .addStringOption(formOption)
      .addStringOption((o) => o.setName('question').setDescription('The question to remove.').setRequired(true).setAutocomplete(true).setMaxLength(100))))
  .addSubcommand((s) => s.setName('panel').setDescription('Post a message with an Apply button.')
    .addStringOption(formOption)
    .addChannelOption((o) => o.setName('channel').setDescription('Where to post it (default: here).').addChannelTypes(...TEXT_CHANNELS)));

// ── Messages ──────────────────────────────────────────────────────────────────

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  FORM_NOT_FOUND: () => refusalEmbed('FORM_NOT_FOUND'),
  NAME_TAKEN: () => errorEmbed('Name Taken', 'Another form already has that name.'),
  FORM_LIMIT: () => errorEmbed('Form Limit Reached', `A server can have up to **${LIMITS.APPLICATION_FORMS}** forms. Delete one first.`),
  CANNOT_USE: (reason) => errorEmbed('Cannot Use That Channel', `${reason} Pick another channel, or fix my permissions there.`),
};

const OFF_NOTE = '\n\n⚠️ The **Applications** module is off, so members cannot apply yet. Turn it on with `/module enable applications`.';

/** Final answer: overwrites the deferred reply. Never pings. */
const finish = (interaction, embed) => safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });

/** The "module is off" note for an admin reply, or '' (also when the settings cannot be loaded). */
async function offNote(guildId) {
  const settings = await getSettingsSafe(guildId);
  return settings && !isModuleEnabled(settings, 'applications') ? OFF_NOTE : '';
}

/** The numbered question list shown after a question changes. */
function questionLines(questions) {
  if (!questions.length) return '*No questions yet. Add one with `/application question add`.*';
  return questions.map((q, i) => `**${i + 1}.** ${clampText(q.label, 60)}${q.required ? '' : ' *(optional)*'}${q.style === 'PARAGRAPH' ? ' ¶' : ''}`).join('\n');
}

async function resolveForm(interaction) {
  const form = await findForm(interaction.guildId, interaction.options.getString('form', true));
  if (!form) await finish(interaction, MSG.FORM_NOT_FOUND());
  return form;
}

/** A channel option must be somewhere the bot can post. Returns the channel, or null after answering. */
async function usableChannel(interaction, option) {
  const picked = interaction.options.getChannel(option);
  if (!picked) return undefined;
  const channel = interaction.guild.channels.cache.get(picked.id) ?? null;
  const usable = checkChannelUsable(channel);
  if (!usable.ok) {
    await finish(interaction, MSG.CANNOT_USE(usable.reason));
    return null;
  }
  return channel;
}

// ── /application form create | edit | delete | list | role ────────────────────

async function runFormCreate(interaction) {
  if (!(await safeDefer(interaction))) return;
  const name = interaction.options.getString('name', true).trim();
  if (!name) return finish(interaction, errorEmbed('Name Needed', 'Give the form a name.'));
  const channel = await usableChannel(interaction, 'review_channel');
  if (channel === null) return undefined;
  const description = interaction.options.getString('description');
  let result;
  try {
    result = await createForm({
      guildId: interaction.guildId, name, description: description?.trim() || null, reviewChannelId: channel?.id ?? null,
    });
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: create failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!result.ok) return finish(interaction, MSG[result.code]());
  logger.info(`[applications] ${interaction.guildId}: form "${name}" created by ${interaction.user.id}`);
  const todo = [
    '`/application question add` for each question',
    channel ? null : '`/application form edit review_channel:` to say where answers go',
    '`/application panel` to post the Apply button',
  ].filter(Boolean).map((t) => `• ${t}`).join('\n');
  return finish(interaction, successEmbed('Form Created', `**${name}** is ready to set up:\n${todo}${await offNote(interaction.guildId)}`));
}

async function runFormEdit(interaction) {
  if (!(await safeDefer(interaction))) return;
  const form = await resolveForm(interaction);
  if (!form) return undefined;
  const { options } = interaction;
  const data = {};
  const changed = [];

  const name = options.getString('name')?.trim();
  if (name !== undefined) {
    if (!name) return finish(interaction, errorEmbed('Name Needed', 'A form needs a name.'));
    data.name = name; changed.push('name');
  }
  const text = (key, column, label) => {
    const raw = options.getString(key);
    if (raw === null) return;
    data[column] = isClearWord(raw) || !raw.trim() ? null : raw.trim();
    changed.push(label);
  };
  text('description', 'description', 'description');
  text('accept_message', 'acceptMessage', 'accept message');
  text('deny_message', 'denyMessage', 'deny message');
  const channel = await usableChannel(interaction, 'review_channel');
  if (channel === null) return undefined;
  if (channel) { data.reviewChannelId = channel.id; changed.push('review channel'); }
  const cooldown = options.getInteger('cooldown_hours');
  if (cooldown !== null) { data.cooldownHours = cooldown; changed.push('cooldown'); }
  const enabled = options.getBoolean('enabled');
  if (enabled !== null) { data.enabled = enabled; changed.push(enabled ? 'opened' : 'closed'); }
  if (!changed.length) return finish(interaction, errorEmbed('Nothing To Change', 'Pick at least one option to change.'));

  let result;
  try {
    result = await updateForm(interaction.guildId, form.id, data);
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: edit failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!result.ok) return finish(interaction, MSG[result.code]());
  await refreshPanel(interaction.guild, result.form);
  return finish(interaction, successEmbed('Form Updated', `**${clampText(result.form.name, 80)}**: changed ${changed.join(', ')}.`));
}

async function runFormDelete(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const form = await resolveForm(interaction);
  if (!form) return undefined;
  const pending = await pendingCount(form.id).catch(() => 0);
  const ok = await awaitConfirm(interaction, {
    title: `Delete "${clampText(form.name, 80)}"?`,
    description: `This deletes the form and all its submissions${pending ? `, including **${pending}** waiting for review` : ''}. This cannot be undone.`,
    confirmLabel: 'Delete', timeoutMs: 30_000,
  });
  if (!ok) return undefined;
  let deleted;
  try {
    deleted = await deleteForm(interaction.guildId, form.id);
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: delete failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (deleted?.panelMessageId) await deleteMessageQuietly(interaction.guild, deleted.panelChannelId, deleted.panelMessageId);
  logger.info(`[applications] ${interaction.guildId}: form "${form.name}" deleted by ${interaction.user.id}`);
  return interaction.editReply({ embeds: [successEmbed('Form Deleted', `**${clampText(form.name, 80)}** and its submissions were deleted.`)], components: [] })
    .catch(() => {});
}

async function runFormList(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  let forms;
  try {
    forms = await listForms(interaction.guildId);
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: list failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  const off = isModuleEnabled(settings, 'applications') ? '' : OFF_NOTE;
  if (!forms.length) return finish(interaction, guildEmbed(settings, '📝 Application forms', `No forms yet. Create one with \`/application form create\`.${off}`));
  const lines = forms.map((f) => {
    const count = parseQuestions(f.questions).length;
    const bits = [
      f.enabled ? '✅ open' : '⏸️ closed',
      count ? `${count} question${count === 1 ? '' : 's'}` : '⚠️ no questions',
      f.reviewChannelId ? `review <#${f.reviewChannelId}>` : '⚠️ no review channel',
      `${f.pending} pending`,
    ];
    if (f.requiredRoleIds.length) bits.push(`needs ${f.requiredRoleIds.map((id) => `<@&${id}>`).join(' ')}`);
    if (f.acceptRoleIds.length) bits.push(`gives ${f.acceptRoleIds.map((id) => `<@&${id}>`).join(' ')}`);
    if (f.panelMessageId) bits.push(`[panel](https://discord.com/channels/${interaction.guildId}/${f.panelChannelId}/${f.panelMessageId})`);
    return `**${clampText(f.name, 60)}** · ${bits.join(' · ')}`;
  });
  const embed = guildEmbed(settings, `📝 Application forms (${forms.length}/${LIMITS.APPLICATION_FORMS})`, off.trim() || undefined);
  chunkLines(lines, 1024).forEach((chunk, i) => embed.addFields({ name: i === 0 ? 'Forms' : '​', value: chunk }));
  return finish(interaction, embed);
}

async function runFormRole(interaction) {
  if (!(await safeDefer(interaction))) return;
  const form = await resolveForm(interaction);
  if (!form) return undefined;
  const which = interaction.options.getString('list', true);
  const column = which === 'accept' ? 'acceptRoleIds' : 'requiredRoleIds';
  const remove = interaction.options.getBoolean('remove') ?? false;
  const picked = interaction.options.getRole('role', true);
  const role = interaction.guild.roles.cache.get(picked.id) ?? null;
  if (!remove) {
    if (!role || role.id === interaction.guildId) return finish(interaction, errorEmbed('Cannot Use That Role', '@everyone cannot be used.'));
    if (which === 'accept') {
      const check = checkAssignableRole(interaction.guild, role, { executor: interaction.member });
      if (!check.ok) return finish(interaction, errorEmbed('Cannot Give That Role', `${check.reason} Accepted members could not receive it.`));
    }
  }
  let result;
  try {
    result = await changeRole(interaction.guildId, form.id, column, picked.id, remove);
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: role change failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!result.ok) {
    if (result.code === 'ROLE_LIMIT') return finish(interaction, errorEmbed('Role Limit Reached', `A list holds up to **${LIMITS.APPLICATION_ROLES}** roles.`));
    return finish(interaction, MSG[result.code]());
  }
  const what = which === 'accept' ? 'given on accept' : 'needed to apply';
  if (!result.changed) {
    return finish(interaction, warnEmbed('No Change', `<@&${picked.id}> ${remove ? 'is not in' : 'is already in'} the ${what} list.`));
  }
  const ids = result.form[column];
  return finish(interaction, successEmbed('Form Updated', `<@&${picked.id}> ${remove ? 'removed from' : 'added to'} the roles ${what} for **${clampText(form.name, 80)}**.\nNow: ${ids.length ? ids.map((id) => `<@&${id}>`).join(' ') : '*none*'}`));
}

// ── /application question add | remove ────────────────────────────────────────

async function runQuestionAdd(interaction) {
  if (!(await safeDefer(interaction))) return;
  const form = await resolveForm(interaction);
  if (!form) return undefined;
  const label = interaction.options.getString('label', true).trim().slice(0, APPLICATION.LABEL_MAX);
  if (!label) return finish(interaction, errorEmbed('Question Needed', 'Write the question.'));
  const placeholder = interaction.options.getString('placeholder')?.trim().slice(0, APPLICATION.PLACEHOLDER_MAX);
  const question = {
    label,
    style: interaction.options.getString('style') === 'PARAGRAPH' ? 'PARAGRAPH' : 'SHORT',
    required: interaction.options.getBoolean('required') ?? true,
    ...(placeholder ? { placeholder } : {}),
  };
  let result;
  try {
    result = await addQuestion(interaction.guildId, form.id, question);
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: question add failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!result.ok) {
    if (result.code === 'QUESTION_LIMIT') return finish(interaction, errorEmbed('Question Limit Reached', `A form holds up to **${LIMITS.APPLICATION_QUESTIONS}** questions.`));
    if (result.code === 'QUESTION_DUP') return finish(interaction, errorEmbed('Duplicate Question', 'The form already has a question with that text.'));
    return finish(interaction, MSG[result.code]());
  }
  await refreshPanel(interaction.guild, result.form);
  return finish(interaction, successEmbed('Question Added', `**${clampText(form.name, 80)}** (${result.count}/${LIMITS.APPLICATION_QUESTIONS}):\n${questionLines(parseQuestions(result.form.questions))}`));
}

async function runQuestionRemove(interaction) {
  if (!(await safeDefer(interaction))) return;
  const form = await resolveForm(interaction);
  if (!form) return undefined;
  const number = Number.parseInt(interaction.options.getString('question', true), 10);
  let result;
  try {
    result = await removeQuestion(interaction.guildId, form.id, number);
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: question remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!result.ok) {
    if (result.code === 'QUESTION_NOT_FOUND') return finish(interaction, errorEmbed('Question Not Found', 'Pick a question from the list.'));
    return finish(interaction, MSG[result.code]());
  }
  await refreshPanel(interaction.guild, result.form);
  return finish(interaction, successEmbed('Question Removed', `Removed "${clampText(result.removed.label, 80)}" from **${clampText(form.name, 80)}**:\n${questionLines(parseQuestions(result.form.questions))}`));
}

// ── /application panel ────────────────────────────────────────────────────────

async function runPanel(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const form = await resolveForm(interaction);
  if (!form) return undefined;
  if (!form.reviewChannelId || parseQuestions(form.questions).length === 0) {
    return finish(interaction, errorEmbed('Not Ready', 'Set a review channel (`/application form edit`) and add at least one question first.'));
  }
  const channel = (await usableChannel(interaction, 'channel')) ?? interaction.channel;
  if (channel === null) return undefined;
  const posted = await postPanel({ guild: interaction.guild, form, channel });
  if (!posted.ok) return finish(interaction, MSG.CANNOT_USE(posted.reason));
  logger.info(`[applications] ${interaction.guildId}: panel for "${form.name}" posted in ${channel.id} by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Panel Posted', `The Apply button for **${clampText(form.name, 80)}** is in <#${channel.id}>.${await offNote(interaction.guildId)}`));
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/**
 * `form` lists the forms (members only get open ones, and only while the module is on); `question` lists the numbered
 * questions of the form already picked. The router gates this exactly like execute, so admin lists reach admins only.
 */
export async function autocomplete(interaction) {
  try {
    const guildId = interaction.guildId;
    const focused = interaction.options.getFocused(true);
    const sub = interaction.options.getSubcommand(false);
    if (focused.name === 'question') {
      const form = await findForm(guildId, interaction.options.getString('form') ?? '');
      const q = String(focused.value ?? '').toLowerCase();
      const questions = form ? parseQuestions(form.questions) : [];
      return respondAutocomplete(interaction, questions
        .map((question, i) => ({ name: `${i + 1}. ${question.label}`, value: String(i + 1) }))
        .filter((c) => !q || c.name.toLowerCase().includes(q)));
    }
    if (focused.name !== 'form') return respondAutocomplete(interaction, []);
    const apply = sub === 'apply';
    if (apply) {
      const settings = peekSettings(guildId)
        ?? await withTimeout(getSettingsSafe(guildId), Math.max(0, ackBudgetMs(interaction)), 'application autocomplete').catch(() => null);
      if (!settings || !isModuleEnabled(settings, 'applications')) return respondAutocomplete(interaction, []);
    }
    const forms = await findFormsForAutocomplete(guildId, focused.value, { enabledOnly: apply });
    return respondAutocomplete(interaction, forms.map((f) => ({ name: `${f.enabled ? '' : '⏸️ '}${f.name}`, value: f.id })));
  } catch (err) {
    logger.debug(`[applications] autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (!group) {
    if (sub === 'apply') return beginApplication(interaction, interaction.options.getString('form', true));
    if (sub === 'panel') return runPanel(interaction);
    return undefined;
  }
  if (group === 'form') {
    if (sub === 'create') return runFormCreate(interaction);
    if (sub === 'edit') return runFormEdit(interaction);
    if (sub === 'delete') return runFormDelete(interaction);
    if (sub === 'list') return runFormList(interaction);
    if (sub === 'role') return runFormRole(interaction);
  }
  if (group === 'question') {
    if (sub === 'add') return runQuestionAdd(interaction);
    if (sub === 'remove') return runQuestionRemove(interaction);
  }
  return undefined;
}
