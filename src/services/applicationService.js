// Applications: admin-built forms (up to 15 questions, 5 per modal page), a member fills them in through modals, and the
// answers are posted to a review channel where a moderator accepts (roles + DM) or denies (reason + DM) them.
// Everything lives here: the pure rules and builders, the DB access for ApplicationForm / ApplicationSubmission, the
// apply flow, the review flow and the cleanup hook bodies. The plugin (src/plugins/applications.js) only declares them
// and /application (src/commands/community/application.js) is a thin shell over them.
// Side-effect free at import: no timers, no I/O. Module-level state is one keyed mutex.
//
// customIds (src/config/customIds.js):
//   application:apply:{formId}                  the panel's Apply button
//   application:next:{submissionId}:{page}      the ephemeral "Continue (2/3)" button (page is 0-based)
//   application:accept:{submissionId}           review buttons
//   application:deny:{submissionId}
//   application-modal:page:{submissionId}:{page}  one page of questions (fields application-q1..q5)
//   application-modal:deny:{submissionId}         the optional deny reason (field application-note)
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, EmbedBuilder, ModalBuilder, TextInputBuilder, TextInputStyle,
} from 'discord.js';
import prisma from '../database/client.js';
import { COLORS, LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { MODULES } from '../config/modules.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled, peekSettings } from './settingsService.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { errorEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { checkAssignableRole, checkChannelUsable, isInteractionGone } from '../utils/interactionHelpers.js';
import { TimeoutError, ackBudgetMs, createKeyedMutex, withTimeout } from '../utils/async.js';
import { clampText } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const APPLICATION = Object.freeze({
  PAGE_SIZE: 5,               // questions per modal page (Discord allows 5 inputs per modal)
  NAME_MAX: 32,               // form name; the modal title "{name} (3/3)" must fit Discord's 45
  LABEL_MAX: 45,              // a question label is a modal input label (Discord max 45)
  PLACEHOLDER_MAX: 100,       // modal input placeholder (Discord max 100)
  DESCRIPTION_MAX: 1000,
  MESSAGE_MAX: 1000,          // acceptMessage / denyMessage
  NOTE_MAX: 500,              // the deny reason
  COOLDOWN_MAX_HOURS: 8760,   // one year
  SHORT_MAX: 300,             // longest answer to a SHORT question
  PARAGRAPH_MAX: 1000,        // longest answer to a PARAGRAPH question
  // All embeds of one message share a 6000 character budget, and a review post has up to 15 answers: every answer is
  // cut to ANSWER_BUDGET / (number of questions), so the post always fits (with room for the outcome field).
  ANSWER_BUDGET: 3800,
  OUTCOME_MAX: 900,           // the "Accepted/Denied by" field added to a reviewed post
  DM_TIMEOUT_MS: 5_000,
  NO_ANSWER: '*No answer*',
  APPLY_LABEL: 'Apply',
  ACCEPT_EMOJI: '✅',
  DENY_EMOJI: '❌',
});

export const DEFAULT_ACCEPT_MESSAGE = 'Your application for **{form}** in **{server}** was accepted. Welcome aboard!';
export const DEFAULT_DENY_MESSAGE = 'Your application for **{form}** in **{server}** was not accepted this time.';

const NO_MENTIONS = Object.freeze({ parse: [] });

// The modal's field ids, one per question slot on a page (registered `field` prefixes in src/config/customIds.js).
const QUESTION_FIELDS = Object.freeze(['application-q1', 'application-q2', 'application-q3', 'application-q4', 'application-q5']);

// Unknown channel / message / member / user, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES = new Set([10003, 10007, 10008, 10013, 50001, 50013]);

const formLock = createKeyedMutex(); // per form (config writes) and per form+member (draft creation)

const isUniqueViolation = (err) => err?.code === 'P2002';

// ── Pure helpers ──────────────────────────────────────────────────────────────

/**
 * The valid questions of a form's JSON column: `{ label, style, required, placeholder? }`, at most the limit.
 * Anything malformed is dropped, so a hand-edited row can never break the modal builder.
 * @param {unknown} raw
 * @returns {Array<{ label: string, style: 'SHORT'|'PARAGRAPH', required: boolean, placeholder?: string }>}
 */
export function parseQuestions(raw) {
  if (!Array.isArray(raw)) return [];
  const out = [];
  for (const q of raw) {
    const label = typeof q?.label === 'string' ? q.label.trim().slice(0, APPLICATION.LABEL_MAX) : '';
    if (!label) continue;
    const placeholder = typeof q.placeholder === 'string' ? q.placeholder.trim().slice(0, APPLICATION.PLACEHOLDER_MAX) : '';
    out.push({
      label,
      style: q.style === 'PARAGRAPH' ? 'PARAGRAPH' : 'SHORT',
      required: q.required !== false,
      ...(placeholder ? { placeholder } : {}),
    });
    if (out.length >= LIMITS.APPLICATION_QUESTIONS) break;
  }
  return out;
}

/** How many modal pages `count` questions take (at least 1). */
export function pageCount(count) {
  return Math.max(1, Math.ceil((Number(count) || 0) / APPLICATION.PAGE_SIZE));
}

/** The questions on a 0-based page. */
export function pageSlice(questions, page) {
  return questions.slice(page * APPLICATION.PAGE_SIZE, (page + 1) * APPLICATION.PAGE_SIZE);
}

/** The longest answer a member may give to a question of `style` on a form with `total` questions. */
export function answerLimit(style, total) {
  const own = style === 'PARAGRAPH' ? APPLICATION.PARAGRAPH_MAX : APPLICATION.SHORT_MAX;
  return Math.max(1, Math.min(own, Math.floor(APPLICATION.ANSWER_BUDGET / Math.max(1, total))));
}

/** Trims an answer and cuts it to `limit` characters. */
export function cleanAnswer(raw, limit) {
  return String(raw ?? '').trim().slice(0, limit);
}

/**
 * The stored answers as one string per current question. An answer only counts for the question that was asked: when
 * an admin renamed or reordered questions after the draft was started, the old answer is dropped.
 * @returns {string[]}
 */
export function normalizeAnswers(questions, stored) {
  const list = Array.isArray(stored) ? stored : [];
  return questions.map((q, i) => (list[i]?.question === q.label && typeof list[i]?.answer === 'string' ? list[i].answer : ''));
}

/** The stored shape `[{ question, answer }]` with the `values` of one page written over `stored`. */
export function mergeAnswers(questions, stored, page, values) {
  const current = normalizeAnswers(questions, stored);
  values.forEach((value, j) => { current[page * APPLICATION.PAGE_SIZE + j] = value; });
  return questions.map((q, i) => ({ question: q.label, answer: current[i] }));
}

/** Indexes of required questions without an answer. */
export function missingRequired(questions, answers) {
  return questions.flatMap((q, i) => (q.required && !String(answers[i] ?? '').trim() ? [i] : []));
}

/** `none`, `clear` or `default` (any case) in an admin text option means "remove it". */
export function isClearWord(value) {
  return /^(none|clear|default)$/i.test(String(value ?? '').trim());
}

/**
 * Whether a member may start (or resume) an application. Pure: the caller loads the facts.
 * @param {{ form: object, memberRoleIds: Iterable<string>, reviewChannelProblem?: string|null,
 *   pending?: object|null, lastDenied?: { reviewedAt: Date|null }|null, now?: Date }} facts
 * @returns {{ ok: true } | { ok: false, code: string, data?: object }}
 */
export function judgeApply({ form, memberRoleIds, reviewChannelProblem = null, pending = null, lastDenied = null, now = new Date() }) {
  if (!form.enabled) return { ok: false, code: 'FORM_DISABLED' };
  if (!form.reviewChannelId || parseQuestions(form.questions).length === 0) return { ok: false, code: 'NOT_SET_UP' };
  if (reviewChannelProblem) return { ok: false, code: 'REVIEW_CHANNEL', data: { reason: reviewChannelProblem } };
  const required = form.requiredRoleIds ?? [];
  if (required.length) {
    const have = new Set(memberRoleIds ?? []);
    if (!required.some((id) => have.has(id))) return { ok: false, code: 'MISSING_ROLE', data: { roleIds: required } };
  }
  if (pending) return { ok: false, code: 'PENDING' };
  if (lastDenied?.reviewedAt && form.cooldownHours > 0) {
    const until = new Date(lastDenied.reviewedAt).getTime() + form.cooldownHours * 3_600_000;
    if (until > now.getTime()) return { ok: false, code: 'COOLDOWN', data: { until: new Date(until) } };
  }
  return { ok: true };
}

const MODULE_OFF_TEXT = (key) => `The **${MODULES[key]?.label ?? key}** module is turned off on this server. An admin can enable it with \`/module enable ${key}\`.`;

/** The warning every member-facing path answers with while the module is off (the router's wording). */
export const moduleOffEmbed = () => warnEmbed('Module Disabled', MODULE_OFF_TEXT('applications'));

/** The embed explaining why an apply attempt was refused. */
export function refusalEmbed(code, data = {}) {
  switch (code) {
    case 'MODULE_OFF': return moduleOffEmbed();
    case 'FORM_NOT_FOUND': return errorEmbed('Application Not Found', 'Pick an application from the list.');
    case 'FORM_DISABLED': return errorEmbed('Applications Closed', 'This application is not open right now.');
    case 'NOT_SET_UP': return errorEmbed('Not Ready', 'This application is not set up yet. An admin needs to set a review channel and add questions.');
    case 'REVIEW_CHANNEL': return errorEmbed('Cannot Apply Right Now', `${data.reason ?? 'The review channel is not usable.'} Please tell an admin.`);
    case 'MISSING_ROLE': return errorEmbed('Not Eligible', `You need one of these roles to apply: ${(data.roleIds ?? []).map((id) => `<@&${id}>`).join(', ')}.`);
    case 'PENDING': return errorEmbed('Already Applied', 'Your application is waiting for review. You will get a message when it has been decided.');
    case 'COOLDOWN': return errorEmbed('Please Wait', `Your last application was denied. You can apply again ${discordTimestamp(data.until, 'R')}.`);
    case 'SLOW': return errorEmbed('Please Try Again', 'That took too long. Please try again in a moment.');
    case 'SERVER_ONLY': return errorEmbed('Server Only', 'Applications can only be used in a server.');
    default: return errorEmbed('Something Went Wrong', 'The application could not be started. Please try again in a moment.');
  }
}

// ── Builders ──────────────────────────────────────────────────────────────────

/** The modal for one page of questions, prefilled with what the member already answered. */
export function buildApplyModal({ form, questions, submission, page }) {
  const total = pageCount(questions.length);
  const title = clampText(total > 1 ? `${form.name} (${page + 1}/${total})` : form.name, 45);
  const saved = normalizeAnswers(questions, submission.answers);
  const modal = new ModalBuilder().setCustomId(buildCustomId('application-modal', 'page', submission.id, page)).setTitle(title);
  pageSlice(questions, page).forEach((q, j) => {
    const limit = answerLimit(q.style, questions.length);
    const input = new TextInputBuilder()
      .setCustomId(QUESTION_FIELDS[j])
      .setLabel(clampText(q.label, APPLICATION.LABEL_MAX))
      .setStyle(q.style === 'PARAGRAPH' ? TextInputStyle.Paragraph : TextInputStyle.Short)
      .setRequired(q.required)
      .setMaxLength(limit);
    if (q.placeholder) input.setPlaceholder(clampText(q.placeholder, APPLICATION.PLACEHOLDER_MAX));
    const previous = saved[page * APPLICATION.PAGE_SIZE + j];
    if (previous) input.setValue(previous.slice(0, limit));
    modal.addComponents(new ActionRowBuilder().addComponents(input));
  });
  return modal;
}

/** The deny form: one optional reason. */
export function buildDenyModal(submissionId) {
  return new ModalBuilder()
    .setCustomId(buildCustomId('application-modal', 'deny', submissionId))
    .setTitle('Deny application')
    .addComponents(new ActionRowBuilder().addComponents(
      new TextInputBuilder().setCustomId('application-note').setLabel('Reason (optional, sent to the applicant)')
        .setStyle(TextInputStyle.Paragraph).setRequired(false).setMaxLength(APPLICATION.NOTE_MAX),
    ));
}

/** The "Continue (2/3)" row: a modal cannot open another modal, so the next page waits behind a button. */
export function continueRow(submissionId, page, total, label = null) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('application', 'next', submissionId, page))
      .setLabel(label ?? `Continue (${page + 1}/${total})`).setEmoji('📝').setStyle(ButtonStyle.Primary),
  );
}

/** Accept / Deny under a review post. */
export function reviewRow(submissionId) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('application', 'accept', submissionId)).setLabel('Accept')
      .setEmoji(APPLICATION.ACCEPT_EMOJI).setStyle(ButtonStyle.Success),
    new ButtonBuilder().setCustomId(buildCustomId('application', 'deny', submissionId)).setLabel('Deny')
      .setEmoji(APPLICATION.DENY_EMOJI).setStyle(ButtonStyle.Danger),
  );
}

/** The panel message with its Apply button. */
export function buildPanelPayload(form, guild = null) {
  const questions = parseQuestions(form.questions);
  const embed = new EmbedBuilder()
    .setColor(COLORS.INFO)
    .setTitle(clampText(`📝 ${form.name}`, 256))
    .setDescription(clampText((form.description && renderPlaceholders(form.description, { guild })) || 'Press **Apply** to fill in the application.', 4000))
    .setFooter({ text: `${questions.length} question${questions.length === 1 ? '' : 's'}` });
  const row = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('application', 'apply', form.id)).setLabel(APPLICATION.APPLY_LABEL)
      .setEmoji('📝').setStyle(ButtonStyle.Primary),
  );
  return { embeds: [embed], components: [row], allowedMentions: NO_MENTIONS };
}

/**
 * The review post: who applied, when their account and membership started, and every answer.
 * @param {{ form: object, answers: Array<{ question: string, answer: string }>, user: object, member?: object|null,
 *   submissionId: string, submittedAt?: Date }} data
 */
export function buildReviewEmbed({ form, answers, user, member = null, submissionId, submittedAt = new Date() }) {
  const lines = [
    `**Applicant:** <@${user.id}> · \`${String(user.username ?? user.id).replace(/`/g, '')}\``,
    `**Account created:** ${discordTimestamp(user.createdAt ?? user.createdTimestamp ?? submittedAt, 'R')}`,
  ];
  if (member?.joinedTimestamp) lines.push(`**Joined the server:** ${discordTimestamp(member.joinedTimestamp, 'R')}`);
  lines.push(`**Submitted:** ${discordTimestamp(submittedAt, 'f')}`);

  const list = Array.isArray(answers) ? answers.slice(0, LIMITS.APPLICATION_QUESTIONS) : [];
  const cap = Math.max(1, Math.min(1024, Math.floor(APPLICATION.ANSWER_BUDGET / Math.max(1, list.length))));
  const embed = new EmbedBuilder()
    .setColor(COLORS.INFO)
    .setTitle(clampText(`📝 ${form.name}`, 256))
    .setDescription(lines.join('\n'))
    .setFooter({ text: `Pending review · ${submissionId}` })
    .setTimestamp(submittedAt);
  const avatar = user.displayAvatarURL?.({ size: 128 });
  if (avatar) embed.setThumbnail(avatar);
  list.forEach((a, i) => {
    const text = String(a?.answer ?? '').trim();
    embed.addFields({
      name: clampText(`${i + 1}. ${a?.question ?? 'Question'}`, 256),
      value: text ? clampText(text, cap) : APPLICATION.NO_ANSWER,
    });
  });
  return embed;
}

/**
 * The review post after a decision: recoloured, the outcome added and the footer changed.
 * @param {import('discord.js').APIEmbed|object|null} source the embed on the message (or null to start a bare one)
 */
export function buildOutcomeEmbed(source, { form, status, reviewerId, note = null, extra = null, submissionId, at = new Date() }) {
  const accepted = status === 'ACCEPTED';
  const embed = source ? EmbedBuilder.from(source) : new EmbedBuilder().setTitle(clampText(`📝 ${form?.name ?? 'Application'}`, 256));
  const head = `${accepted ? APPLICATION.ACCEPT_EMOJI : APPLICATION.DENY_EMOJI} **${accepted ? 'Accepted' : 'Denied'}** by <@${reviewerId}> ${discordTimestamp(at, 'R')}`;
  const value = [head, note ? `**Reason:** ${note}` : null, extra].filter(Boolean).join('\n');
  embed.setColor(accepted ? COLORS.SUCCESS : COLORS.ERROR)
    .setFooter({ text: `${accepted ? 'Accepted' : 'Denied'} · ${submissionId}` })
    .addFields({ name: 'Outcome', value: clampText(value, APPLICATION.OUTCOME_MAX) });
  return embed;
}

// ── Settings and permission helpers ───────────────────────────────────────────

/** Memo first, then a lookup bounded by what is left of the 3 s acknowledgement window. */
async function settingsWithinBudget(interaction) {
  const memo = peekSettings(interaction.guildId);
  if (memo) return memo;
  const budget = ackBudgetMs(interaction);
  if (budget <= 0) return null;
  return withTimeout(getSettingsSafe(interaction.guildId), budget, 'application settings').catch(() => null);
}

const reply = (interaction, embed, extra = {}) => interaction.reply({ embeds: [embed], ephemeral: true, allowedMentions: NO_MENTIONS, ...extra })
  .catch((err) => { if (!isInteractionGone(err)) logger.warn(`[applications] reply failed: ${err?.message}`); });

/** Answers a deferred interaction, whatever it was deferred as (reply or update). Never throws. */
async function answer(interaction, embed, components = []) {
  try {
    await interaction.editReply({ embeds: [embed], components, allowedMentions: NO_MENTIONS });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`[applications] could not answer: ${err?.message}`);
  }
}

const followUp = (interaction, embed) => interaction.followUp({ embeds: [embed], ephemeral: true, allowedMentions: NO_MENTIONS })
  .catch((err) => { if (!isInteractionGone(err)) logger.warn(`[applications] follow-up failed: ${err?.message}`); });

// ── Forms (DB) ────────────────────────────────────────────────────────────────

/** Every form of a guild with its number of submissions waiting for review. */
export async function listForms(guildId) {
  const forms = await prisma.applicationForm.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' } });
  if (!forms.length) return [];
  const counts = await prisma.applicationSubmission.groupBy({ by: ['formId'], where: { guildId, status: 'PENDING', reviewMessageId: { not: null } }, _count: { _all: true } });
  const pending = new Map(counts.map((c) => [c.formId, c._count?._all ?? 0]));
  return forms.map((f) => ({ ...f, pending: pending.get(f.id) ?? 0 }));
}

/** A form of this guild by id, or else by name (case-insensitive). */
export async function findForm(guildId, ref) {
  const text = String(ref ?? '').trim().slice(0, 100);
  if (!text) return null;
  const byId = await prisma.applicationForm.findFirst({ where: { id: text, guildId } });
  if (byId) return byId;
  return prisma.applicationForm.findFirst({ where: { guildId, name: { equals: text, mode: 'insensitive' } } });
}

/** Choices for the `form` option: id values, names as labels. */
export async function findFormsForAutocomplete(guildId, query, { enabledOnly = false } = {}) {
  const q = String(query ?? '').trim().slice(0, 100);
  return prisma.applicationForm.findMany({
    where: { guildId, ...(enabledOnly ? { enabled: true } : {}), ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}) },
    orderBy: { createdAt: 'asc' },
    take: 25,
    select: { id: true, name: true, enabled: true },
  });
}

/**
 * Creates a form. The cap and the case-insensitive name check run under the guild's lock.
 * @returns {Promise<{ ok: true, form: object } | { ok: false, code: 'FORM_LIMIT'|'NAME_TAKEN' }>}
 * @throws on database failure
 */
export async function createForm({ guildId, name, description = null, reviewChannelId = null }) {
  await ensureGuildSettings(guildId);
  return formLock.run(`guild:${guildId}`, async () => {
    if (await prisma.applicationForm.count({ where: { guildId } }) >= LIMITS.APPLICATION_FORMS) return { ok: false, code: 'FORM_LIMIT' };
    const clash = await prisma.applicationForm.findFirst({ where: { guildId, name: { equals: name, mode: 'insensitive' } }, select: { id: true } });
    if (clash) return { ok: false, code: 'NAME_TAKEN' };
    try {
      const form = await prisma.applicationForm.create({ data: { guildId, name, description, reviewChannelId } });
      return { ok: true, form };
    } catch (err) {
      if (isUniqueViolation(err)) return { ok: false, code: 'NAME_TAKEN' };
      throw err;
    }
  });
}

/**
 * Writes columns of one form (guild-scoped). A rename that collides case-insensitively is refused.
 * @returns {Promise<{ ok: true, form: object } | { ok: false, code: 'FORM_NOT_FOUND'|'NAME_TAKEN' }>}
 * @throws on database failure
 */
export async function updateForm(guildId, formId, data) {
  return formLock.run(`form:${formId}`, async () => {
    if (typeof data.name === 'string') {
      const clash = await prisma.applicationForm.findFirst({
        where: { guildId, name: { equals: data.name, mode: 'insensitive' }, NOT: { id: formId } }, select: { id: true },
      });
      if (clash) return { ok: false, code: 'NAME_TAKEN' };
    }
    try {
      const res = await prisma.applicationForm.updateMany({ where: { id: formId, guildId }, data });
      if (res.count !== 1) return { ok: false, code: 'FORM_NOT_FOUND' };
    } catch (err) {
      if (isUniqueViolation(err)) return { ok: false, code: 'NAME_TAKEN' };
      throw err;
    }
    const form = await prisma.applicationForm.findFirst({ where: { id: formId, guildId } });
    return form ? { ok: true, form } : { ok: false, code: 'FORM_NOT_FOUND' };
  });
}

/** Deletes a form; its submissions go with it (cascade). Returns the deleted row, or null. */
export async function deleteForm(guildId, formId) {
  const form = await prisma.applicationForm.findFirst({ where: { id: formId, guildId } });
  if (!form) return null;
  await prisma.applicationForm.deleteMany({ where: { id: formId, guildId } });
  return form;
}

/**
 * Appends a question (cap and duplicate label checked under the form's lock).
 * @returns {Promise<{ ok: true, form: object, count: number } | { ok: false, code: 'FORM_NOT_FOUND'|'QUESTION_LIMIT'|'QUESTION_DUP' }>}
 */
export async function addQuestion(guildId, formId, question) {
  return formLock.run(`form:${formId}`, async () => {
    const form = await prisma.applicationForm.findFirst({ where: { id: formId, guildId } });
    if (!form) return { ok: false, code: 'FORM_NOT_FOUND' };
    const questions = parseQuestions(form.questions);
    if (questions.length >= LIMITS.APPLICATION_QUESTIONS) return { ok: false, code: 'QUESTION_LIMIT' };
    const lower = question.label.toLowerCase();
    if (questions.some((q) => q.label.toLowerCase() === lower)) return { ok: false, code: 'QUESTION_DUP' };
    questions.push(question);
    await prisma.applicationForm.updateMany({ where: { id: formId, guildId }, data: { questions } });
    return { ok: true, form: { ...form, questions }, count: questions.length };
  });
}

/**
 * Removes question number `number` (1-based).
 * @returns {Promise<{ ok: true, form: object, removed: object } | { ok: false, code: 'FORM_NOT_FOUND'|'QUESTION_NOT_FOUND' }>}
 */
export async function removeQuestion(guildId, formId, number) {
  return formLock.run(`form:${formId}`, async () => {
    const form = await prisma.applicationForm.findFirst({ where: { id: formId, guildId } });
    if (!form) return { ok: false, code: 'FORM_NOT_FOUND' };
    const questions = parseQuestions(form.questions);
    if (!Number.isInteger(number) || number < 1 || number > questions.length) return { ok: false, code: 'QUESTION_NOT_FOUND' };
    const [removed] = questions.splice(number - 1, 1);
    await prisma.applicationForm.updateMany({ where: { id: formId, guildId }, data: { questions } });
    return { ok: true, form: { ...form, questions }, removed };
  });
}

/**
 * Adds or removes a role in `acceptRoleIds` or `requiredRoleIds` (cap checked under the form's lock).
 * @param {'acceptRoleIds'|'requiredRoleIds'} column
 * @returns {Promise<{ ok: true, form: object, changed: boolean } | { ok: false, code: 'FORM_NOT_FOUND'|'ROLE_LIMIT' }>}
 */
export async function changeRole(guildId, formId, column, roleId, remove) {
  return formLock.run(`form:${formId}`, async () => {
    const form = await prisma.applicationForm.findFirst({ where: { id: formId, guildId } });
    if (!form) return { ok: false, code: 'FORM_NOT_FOUND' };
    const ids = [...(form[column] ?? [])];
    const has = ids.includes(roleId);
    if (remove ? !has : has) return { ok: true, form, changed: false };
    if (!remove && ids.length >= LIMITS.APPLICATION_ROLES) return { ok: false, code: 'ROLE_LIMIT' };
    const next = remove ? ids.filter((id) => id !== roleId) : [...ids, roleId];
    await prisma.applicationForm.updateMany({ where: { id: formId, guildId }, data: { [column]: next } });
    return { ok: true, form: { ...form, [column]: next }, changed: true };
  });
}

// ── The panel ─────────────────────────────────────────────────────────────────

/**
 * Posts (or replaces) the Apply panel of a form in `channel`. The old panel message is deleted best-effort.
 * @returns {Promise<{ ok: true, message: object } | { ok: false, reason: string }>}
 */
export async function postPanel({ guild, form, channel }) {
  const usable = checkChannelUsable(channel);
  if (!usable.ok) return { ok: false, reason: usable.reason };
  let message;
  try {
    message = await channel.send(buildPanelPayload(form, guild));
  } catch (err) {
    logger.warn(`[applications] ${guild.id}: could not post the panel: ${err?.message}`);
    return { ok: false, reason: 'I could not post in that channel.' };
  }
  const old = { channelId: form.panelChannelId, messageId: form.panelMessageId };
  try {
    await prisma.applicationForm.updateMany({
      where: { id: form.id, guildId: guild.id }, data: { panelChannelId: channel.id, panelMessageId: message.id },
    });
  } catch (err) {
    logger.error(`[applications] ${guild.id}: could not save the panel ids`, err);
    await message.delete().catch(() => {});
    return { ok: false, reason: 'The panel could not be saved. Please try again in a moment.' };
  }
  if (old.messageId && old.messageId !== message.id) await deleteMessageQuietly(guild, old.channelId, old.messageId);
  return { ok: true, message };
}

/** Re-renders the form's posted panel (name, description and question count may have changed). Never throws. */
export async function refreshPanel(guild, form) {
  if (!form.panelChannelId || !form.panelMessageId) return;
  try {
    const channel = guild.channels.cache.get(form.panelChannelId);
    if (!channel?.isTextBased?.()) return;
    const { embeds, components } = buildPanelPayload(form, guild);
    await channel.messages.edit(form.panelMessageId, { embeds, components });
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.debug(`[applications] could not refresh a panel: ${err?.message}`);
  }
}

/** How many submissions of a form wait for review (orphaned ones, whose review post is gone, are not counted). */
export function pendingCount(formId) {
  return prisma.applicationSubmission.count({ where: { formId, status: 'PENDING', reviewMessageId: { not: null } } });
}

/** Deletes a message by ids; a missing channel or message is fine. Never throws. */
export async function deleteMessageQuietly(guild, channelId, messageId) {
  try {
    const channel = channelId ? (guild.channels.cache.get(channelId) ?? await guild.channels.fetch(channelId).catch(() => null)) : null;
    if (channel?.isTextBased?.()) await channel.messages.delete(messageId);
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.debug(`[applications] could not delete message ${messageId}: ${err?.message}`);
  }
}

// ── Applying ──────────────────────────────────────────────────────────────────

/**
 * Finds or creates the member's DRAFT for a form after the eligibility checks (no PENDING one, past the cooldown),
 * under a per member+form lock so two clicks cannot create two drafts.
 * A PENDING row without a review message (it was deleted, or its channel was) is orphaned and does not block.
 * @returns {Promise<{ ok: true, submission: object } | { ok: false, code: string, data?: object }>}
 */
export async function startDraft({ form, userId, memberRoleIds, reviewChannelProblem = null, now = new Date() }) {
  const early = judgeApply({ form, memberRoleIds, reviewChannelProblem, now });
  if (!early.ok) return early;
  return formLock.run(`draft:${form.id}:${userId}`, async () => {
    const mine = { formId: form.id, userId };
    const [pending, lastDenied, draft] = await Promise.all([
      prisma.applicationSubmission.findFirst({ where: { ...mine, status: 'PENDING', reviewMessageId: { not: null } }, select: { id: true } }),
      form.cooldownHours > 0
        ? prisma.applicationSubmission.findFirst({ where: { ...mine, status: 'DENIED', reviewedAt: { not: null } }, orderBy: { reviewedAt: 'desc' }, select: { reviewedAt: true } })
        : null,
      prisma.applicationSubmission.findFirst({ where: { ...mine, status: 'DRAFT' } }),
    ]);
    const verdict = judgeApply({ form, memberRoleIds, reviewChannelProblem, pending, lastDenied, now });
    if (!verdict.ok) return verdict;
    if (draft) return { ok: true, submission: draft };
    await ensureGuildSettings(form.guildId);
    const submission = await prisma.applicationSubmission.create({ data: { guildId: form.guildId, formId: form.id, userId } });
    return { ok: true, submission };
  });
}

/** Why the bot cannot post in the form's review channel (null when it can, or when none is set: that is NOT_SET_UP). */
function reviewChannelIssue(guild, form) {
  if (!form.reviewChannelId) return null;
  const channel = guild.channels.cache.get(form.reviewChannelId);
  if (!channel) return 'The review channel was deleted.';
  const usable = checkChannelUsable(channel);
  return usable.ok ? null : usable.reason;
}

/**
 * Starts an application: every check, then page 1 of the form as a modal (the first and only response).
 * Used by `/application apply` (a name or id) and by the panel's Apply button (an id).
 * @param {import('discord.js').ChatInputCommandInteraction|import('discord.js').ButtonInteraction} interaction
 * @param {string} formRef
 */
export async function beginApplication(interaction, formRef) {
  if (!interaction.inCachedGuild()) return reply(interaction, refusalEmbed('SERVER_ONLY'));
  const { guild, member } = interaction;
  const budget = ackBudgetMs(interaction);
  if (budget <= 0) return reply(interaction, refusalEmbed('SLOW'));

  let outcome;
  try {
    outcome = await withTimeout((async () => {
      const settings = peekSettings(guild.id) ?? await getSettingsSafe(guild.id);
      if (!settings) return { ok: false, code: 'SLOW' };
      if (!isModuleEnabled(settings, 'applications')) return { ok: false, code: 'MODULE_OFF' };
      const form = await findForm(guild.id, formRef);
      if (!form) return { ok: false, code: 'FORM_NOT_FOUND' };
      const problem = reviewChannelIssue(guild, form);
      const started = await startDraft({ form, userId: interaction.user.id, memberRoleIds: [...member.roles.cache.keys()], reviewChannelProblem: problem });
      return started.ok ? { ...started, form } : started;
    })(), budget, 'application start');
  } catch (err) {
    if (!(err instanceof TimeoutError)) logger.error(`[applications] ${guild.id}: could not start an application`, err);
    return reply(interaction, refusalEmbed(err instanceof TimeoutError ? 'SLOW' : 'ERROR'));
  }
  if (!outcome.ok) return reply(interaction, refusalEmbed(outcome.code, outcome.data));

  try {
    await interaction.showModal(buildApplyModal({ form: outcome.form, questions: parseQuestions(outcome.form.questions), submission: outcome.submission, page: 0 }));
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`[applications] ${guild.id}: showModal failed: ${err?.message}`);
  }
  return undefined;
}

/** The Continue button: opens page N of the member's own draft. */
async function handleNext(interaction, submissionId, pageRaw) {
  const budget = ackBudgetMs(interaction);
  if (budget <= 0) return reply(interaction, refusalEmbed('SLOW'));
  let submission;
  try {
    submission = await withTimeout(prisma.applicationSubmission.findFirst({
      where: { id: submissionId, guildId: interaction.guildId, userId: interaction.user.id, status: 'DRAFT' },
      include: { form: true },
    }), budget, 'application draft');
  } catch (err) {
    if (!(err instanceof TimeoutError)) logger.warn(`[applications] draft lookup failed: ${err?.message}`);
    return reply(interaction, refusalEmbed(err instanceof TimeoutError ? 'SLOW' : 'ERROR'));
  }
  const questions = submission ? parseQuestions(submission.form.questions) : [];
  const page = Number(pageRaw);
  if (!submission || !Number.isInteger(page) || page < 0 || page >= pageCount(questions.length)) {
    return reply(interaction, errorEmbed('Application Closed', 'This application is no longer open. Start again with the Apply button or `/application apply`.'));
  }
  if (!submission.form.enabled) return reply(interaction, refusalEmbed('FORM_DISABLED'));
  try {
    await interaction.showModal(buildApplyModal({ form: submission.form, questions, submission, page }));
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`[applications] showModal failed: ${err?.message}`);
  }
  return undefined;
}

/**
 * One submitted page: saves the answers, then either asks for the next page or finishes the application.
 * Page 1 is acknowledged with a new ephemeral reply (it may come from the public panel button, so `update` would
 * overwrite the panel); later pages come from the ephemeral Continue message and replace it.
 */
async function handlePageSubmit(interaction, client, submissionId, pageRaw) {
  const page = Number(pageRaw);
  try {
    if (page > 0 && interaction.isFromMessage?.()) await interaction.deferUpdate();
    else await interaction.deferReply({ ephemeral: true });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`[applications] acknowledging a page failed: ${err?.message}`);
    return undefined;
  }

  let submission;
  try {
    submission = await prisma.applicationSubmission.findFirst({
      where: { id: submissionId, guildId: interaction.guildId, userId: interaction.user.id, status: 'DRAFT' },
      include: { form: true },
    });
  } catch (err) {
    logger.warn(`[applications] draft lookup failed: ${err?.message}`);
    return answer(interaction, refusalEmbed('ERROR'));
  }
  const form = submission?.form;
  const questions = form ? parseQuestions(form.questions) : [];
  const pages = pageCount(questions.length);
  if (!submission || !Number.isInteger(page) || page < 0 || page >= pages) {
    return answer(interaction, errorEmbed('Application Closed', 'This application is no longer open or was already submitted.'));
  }
  if (!form.enabled) return answer(interaction, refusalEmbed('FORM_DISABLED'));

  // Read this page's fields. Every answer is saved, even when a required one is empty, so a retry is prefilled.
  const values = pageSlice(questions, page).map((q, j) => {
    let raw = '';
    try { raw = interaction.fields.getTextInputValue(QUESTION_FIELDS[j]); } catch { /* not in the form: empty */ }
    return cleanAnswer(raw, answerLimit(q.style, questions.length));
  });
  const merged = mergeAnswers(questions, submission.answers, page, values);
  try {
    const res = await prisma.applicationSubmission.updateMany({
      where: { id: submission.id, userId: interaction.user.id, status: 'DRAFT' }, data: { answers: merged },
    });
    if (res.count !== 1) return answer(interaction, errorEmbed('Application Closed', 'This application is no longer open or was already submitted.'));
  } catch (err) {
    logger.error(`[applications] ${interaction.guildId}: could not save answers`, err);
    return answer(interaction, errorEmbed('Could Not Save', 'Your answers could not be saved. Please press the button and try again.'), [continueRow(submission.id, page, pages, 'Try again')]);
  }

  const answers = merged.map((a) => a.answer);
  const missing = missingRequired(questions, answers);
  const missingHere = missing.filter((i) => Math.floor(i / APPLICATION.PAGE_SIZE) === page);
  if (missingHere.length) {
    const names = missingHere.map((i) => `• ${questions[i].label}`).join('\n');
    return answer(interaction, errorEmbed('Answer Required', `These questions need an answer:\n${names}`), [continueRow(submission.id, page, pages, 'Fix answers')]);
  }
  if (page < pages - 1) {
    return answer(interaction, successEmbed(`Page ${page + 1} of ${pages} saved`, 'Press the button for the next questions.'), [continueRow(submission.id, page + 1, pages)]);
  }
  if (missing.length) {
    // Questions were added after earlier pages were answered: send the member to the first page with a gap.
    const first = Math.floor(missing[0] / APPLICATION.PAGE_SIZE);
    return answer(interaction, errorEmbed('Answer Required', `This form changed. These questions still need an answer:\n${missing.map((i) => `• ${questions[i].label}`).join('\n')}`), [continueRow(submission.id, first, pages, 'Fix answers')]);
  }
  return finishApplication(interaction, client, { submission, form, answers: merged });
}

/** Moves a complete draft to PENDING and posts it for review; reverts to DRAFT when it cannot be posted. */
async function finishApplication(interaction, client, { submission, form, answers }) {
  const { guild } = interaction;
  const channel = form.reviewChannelId ? guild.channels.cache.get(form.reviewChannelId) : null;
  const problem = reviewChannelIssue(guild, form);
  if (!form.reviewChannelId) return answer(interaction, refusalEmbed('NOT_SET_UP'));
  if (problem) return answer(interaction, refusalEmbed('REVIEW_CHANNEL', { reason: problem }));

  const submittedAt = new Date();
  try {
    // The claim: only one concurrent submit can move this draft to PENDING, so a double click posts once.
    const claim = await prisma.applicationSubmission.updateMany({
      where: { id: submission.id, userId: interaction.user.id, status: 'DRAFT' }, data: { status: 'PENDING', submittedAt },
    });
    if (claim.count !== 1) return answer(interaction, errorEmbed('Already Submitted', 'This application was already submitted.'));
  } catch (err) {
    logger.error(`[applications] ${guild.id}: could not submit`, err);
    return answer(interaction, refusalEmbed('ERROR'));
  }

  let message;
  try {
    message = await channel.send({
      embeds: [buildReviewEmbed({ form, answers, user: interaction.user, member: interaction.member, submissionId: submission.id, submittedAt })],
      components: [reviewRow(submission.id)],
      allowedMentions: NO_MENTIONS,
    });
  } catch (err) {
    logger.warn(`[applications] ${guild.id}: could not post the review message: ${err?.message}`);
    await prisma.applicationSubmission.updateMany({ where: { id: submission.id, status: 'PENDING' }, data: { status: 'DRAFT', submittedAt: null } })
      .catch((e) => logger.error(`[applications] ${guild.id}: could not revert a submission`, e));
    return answer(interaction, refusalEmbed('REVIEW_CHANNEL', { reason: 'I could not post in the review channel.' }));
  }
  await prisma.applicationSubmission.updateMany({ where: { id: submission.id }, data: { reviewMessageId: message.id } })
    .catch((err) => logger.error(`[applications] ${guild.id}: could not save the review message id`, err));
  logger.info(`[applications] ${guild.id}: ${interaction.user.id} submitted "${form.name}" (${submission.id})`);
  return answer(interaction, successEmbed('Application Submitted', `Your application for **${form.name}** was sent for review. You will get a message when it has been decided.`));
}

// ── Reviewing ─────────────────────────────────────────────────────────────────

/** Gives the accept roles; reports what worked and what did not (deleted, above the bot, managed, dangerous...). */
export async function giveAcceptRoles({ guild, member, roleIds, reason }) {
  const given = [];
  const already = [];
  const failed = [];
  for (const id of roleIds) {
    const role = guild.roles.cache.get(id);
    if (!role) { failed.push({ id, reason: 'the role no longer exists' }); continue; }
    if (member.roles.cache.has(id)) { already.push(id); continue; }
    const check = checkAssignableRole(guild, role);
    if (!check.ok) { failed.push({ id, reason: check.reason }); continue; }
    try {
      await member.roles.add(role, clampText(reason, 512));
      given.push(id);
    } catch (err) {
      failed.push({ id, reason: err?.code === 50013 ? 'I am missing permissions' : (err?.message ?? 'unknown error') });
    }
  }
  return { given, already, failed };
}

/** DMs the applicant; never throws. `ok: false` carries a short reason for the reviewer. */
async function sendDecisionDm({ client, userId, user, form, guild, member, status, note }) {
  const accepted = status === 'ACCEPTED';
  // The user first, so {user...} tokens resolve even when the applicant has left the server.
  const target = user ?? await Promise.resolve().then(() => client.users.fetch(userId)).catch(() => null);
  if (!target) return { ok: false, reason: 'the user could not be found' };
  const template = (accepted ? form.acceptMessage : form.denyMessage) || (accepted ? DEFAULT_ACCEPT_MESSAGE : DEFAULT_DENY_MESSAGE);
  let text = renderPlaceholders(template, { member, user: target, guild, vars: { form: form.name, reason: note ?? '' } }, { maxLength: 3500 });
  // A deny message that places {reason} itself already shows it.
  if (!accepted && note && !/\{reason\}/i.test(template)) text += `\n\n**Reason:** ${note}`;
  const embed = (accepted ? successEmbed('Application accepted') : errorEmbed('Application denied')).setDescription(clampText(text, 4000))
    .setFooter({ text: clampText(guild.name, 100) });
  try {
    await withTimeout(target.send({ embeds: [embed], allowedMentions: NO_MENTIONS }), APPLICATION.DM_TIMEOUT_MS, 'application DM');
    return { ok: true };
  } catch (err) {
    return { ok: false, reason: err?.code === 50007 ? 'their DMs are closed' : 'the message could not be sent' };
  }
}

/** Whether the clicker may review (MODERATOR or above; the router does not check buttons and modals). */
async function reviewerAllowed(interaction) {
  const settings = await settingsWithinBudget(interaction);
  // ADMIN follows from Discord permissions and a moderator role from the settings, so unavailable settings (null)
  // can never wrongly grant either.
  return resolvePermissionLevel(interaction.member, settings) >= PERMISSION_LEVELS.MODERATOR;
}

const NOT_REVIEWER = () => errorEmbed('Insufficient Permissions', 'Only moderators can review applications.');

/** The Accept / Deny buttons under a review post. */
async function handleReviewButton(interaction, client, action, submissionId) {
  if (!interaction.inCachedGuild()) return reply(interaction, refusalEmbed('SERVER_ONLY'));
  if (!(await reviewerAllowed(interaction))) return reply(interaction, NOT_REVIEWER());
  if (action === 'deny') {
    try {
      await interaction.showModal(buildDenyModal(submissionId));
    } catch (err) {
      if (!isInteractionGone(err)) logger.warn(`[applications] showModal failed: ${err?.message}`);
    }
    return undefined;
  }
  try {
    await interaction.deferUpdate();
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`[applications] acknowledging a review failed: ${err?.message}`);
    return undefined;
  }
  return decide(interaction, client, { submissionId, status: 'ACCEPTED', note: null });
}

/** The deny reason form. */
async function handleDenySubmit(interaction, client, submissionId) {
  if (!interaction.inCachedGuild()) return reply(interaction, refusalEmbed('SERVER_ONLY'));
  if (!(await reviewerAllowed(interaction))) return reply(interaction, NOT_REVIEWER());
  let note = '';
  try { note = interaction.fields.getTextInputValue('application-note'); } catch { /* no reason */ }
  try {
    if (interaction.isFromMessage?.()) await interaction.deferUpdate();
    else await interaction.deferReply({ ephemeral: true });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`[applications] acknowledging a denial failed: ${err?.message}`);
    return undefined;
  }
  return decide(interaction, client, { submissionId, status: 'DENIED', note: cleanAnswer(note, APPLICATION.NOTE_MAX) || null });
}

/**
 * Records a decision on a PENDING submission: claims it (so two reviewers cannot both decide), gives roles or sends
 * the DM, and edits the review post. Runs after the interaction was acknowledged with deferUpdate.
 */
async function decide(interaction, client, { submissionId, status, note }) {
  const { guild } = interaction;
  const reviewer = interaction.user;
  let submission;
  try {
    submission = await prisma.applicationSubmission.findFirst({ where: { id: submissionId, guildId: guild.id }, include: { form: true } });
  } catch (err) {
    logger.warn(`[applications] ${guild.id}: review lookup failed: ${err?.message}`);
    return followUp(interaction, refusalEmbed('ERROR'));
  }
  if (!submission) {
    await editReviewMessage(interaction, { components: [] });
    return followUp(interaction, errorEmbed('Application Gone', 'This application no longer exists (its form was deleted).'));
  }
  if (submission.userId === reviewer.id) return followUp(interaction, errorEmbed('Not Allowed', 'You cannot review your own application.'));
  if (submission.status !== 'PENDING') {
    await editReviewMessage(interaction, { components: [] });
    return followUp(interaction, errorEmbed('Already Reviewed', `This application was already ${submission.status.toLowerCase()}${submission.reviewerId ? ` by <@${submission.reviewerId}>` : ''}.`));
  }

  const now = new Date();
  let claim;
  try {
    claim = await prisma.applicationSubmission.updateMany({
      where: { id: submission.id, status: 'PENDING' },
      data: { status, reviewerId: reviewer.id, reviewedAt: now, reviewNote: note },
    });
  } catch (err) {
    logger.error(`[applications] ${guild.id}: could not record a review`, err);
    return followUp(interaction, refusalEmbed('ERROR'));
  }
  if (claim.count !== 1) return followUp(interaction, errorEmbed('Already Reviewed', 'Someone else just reviewed this application.'));

  const form = submission.form;
  const accepted = status === 'ACCEPTED';
  const member = await guild.members.fetch(submission.userId).catch(() => null);
  const problems = [];
  let extra = null;

  if (accepted && form.acceptRoleIds.length) {
    if (!member) {
      problems.push('The applicant is no longer in the server, so no roles were given.');
    } else {
      const roles = await giveAcceptRoles({ guild, member, roleIds: form.acceptRoleIds, reason: `Application accepted: ${form.name}` });
      const parts = [];
      if (roles.given.length) parts.push(`Roles given: ${roles.given.map((id) => `<@&${id}>`).join(' ')}`);
      if (roles.already.length) parts.push(`Already had: ${roles.already.map((id) => `<@&${id}>`).join(' ')}`);
      for (const f of roles.failed) {
        const line = `<@&${f.id}>: ${f.reason}`;
        problems.push(`Could not give ${line}`);
        parts.push(`⚠️ Could not give ${line}`);
      }
      extra = parts.join('\n') || null;
    }
  }

  const dm = await sendDecisionDm({
    client, userId: submission.userId, user: member?.user ?? null, form, guild, member, status, note,
  });
  if (!dm.ok) problems.push(`The applicant was not messaged: ${dm.reason}.`);
  if (!member && !accepted) problems.push('The applicant is no longer in the server.');

  const source = interaction.message?.embeds?.[0] ?? null;
  await editReviewMessage(interaction, {
    embeds: [buildOutcomeEmbed(source, { form, status, reviewerId: reviewer.id, note, extra, submissionId: submission.id, at: now })],
    components: [],
    allowedMentions: NO_MENTIONS,
  });
  logger.info(`[applications] ${guild.id}: ${reviewer.id} ${accepted ? 'accepted' : 'denied'} ${submission.id}`);
  if (problems.length) await followUp(interaction, warnEmbed('Decision Recorded', problems.join('\n')));
  return undefined;
}

/** Edits the review post, after the interaction was deferred as an update. Never throws. */
async function editReviewMessage(interaction, payload) {
  try {
    if (interaction.isFromMessage?.() ?? true) await interaction.editReply(payload);
    else await interaction.message?.edit(payload);
  } catch (err) {
    if (!GONE_CODES.has(err?.code) && !isInteractionGone(err)) logger.warn(`[applications] could not edit the review post: ${err?.message}`);
  }
}

// ── Component handlers (registered by the plugin) ─────────────────────────────

/** `application:{apply|next|accept|deny}:...` */
export async function handleButton(interaction, client) {
  const { params } = parseCustomId(interaction.customId);
  const [action, id, page] = params;
  if (action === 'apply') return beginApplication(interaction, id);
  if (!interaction.inCachedGuild()) return reply(interaction, refusalEmbed('SERVER_ONLY'));
  if (action === 'next') return handleNext(interaction, id, page);
  if (action === 'accept' || action === 'deny') return handleReviewButton(interaction, client, action, id);
  return reply(interaction, errorEmbed('Button Expired', 'This button is no longer active.'));
}

/** `application-modal:page:{submissionId}:{page}` and `application-modal:deny:{submissionId}` */
export async function handleModal(interaction, client) {
  const { params } = parseCustomId(interaction.customId);
  const [kind, id, page] = params;
  if (!interaction.inCachedGuild()) return reply(interaction, refusalEmbed('SERVER_ONLY'));
  if (kind === 'page') return handlePageSubmit(interaction, client, id, page);
  if (kind === 'deny') return handleDenySubmit(interaction, client, id);
  return reply(interaction, errorEmbed('Form Expired', 'This form has expired. Please start again.'));
}

// ── Cleanup hooks (registered `always`: dead references must go while the module is off too) ──

/** A bot message was deleted: forget it as a panel or as a review post. */
export async function onMessageDelete(ctx) {
  const id = ctx.message?.id;
  const guildId = ctx.guildId;
  if (!id || !guildId) return;
  try {
    await prisma.applicationForm.updateMany({ where: { guildId, panelMessageId: id }, data: { panelMessageId: null, panelChannelId: null } });
    await prisma.applicationSubmission.updateMany({ where: { guildId, reviewMessageId: id }, data: { reviewMessageId: null } });
  } catch (err) {
    logger.warn(`[applications] ${guildId}: message cleanup failed: ${err?.message}`);
  }
}

/**
 * A channel was deleted: its forms lose the review channel (they cannot be opened until an admin sets another), their
 * review posts are gone with it, and panels in it are forgotten. Order matters: the review posts are found through
 * the forms that still name the channel.
 */
export async function onChannelDelete(ctx) {
  const id = ctx.channel?.id;
  const guildId = ctx.guildId;
  if (!id || !guildId) return;
  try {
    const forms = await prisma.applicationForm.findMany({
      where: { guildId, OR: [{ reviewChannelId: id }, { panelChannelId: id }] }, select: { id: true, reviewChannelId: true, panelChannelId: true },
    });
    if (!forms.length) return;
    const reviewIds = forms.filter((f) => f.reviewChannelId === id).map((f) => f.id);
    const panelIds = forms.filter((f) => f.panelChannelId === id).map((f) => f.id);
    if (reviewIds.length) {
      await prisma.applicationSubmission.updateMany({ where: { formId: { in: reviewIds }, reviewMessageId: { not: null } }, data: { reviewMessageId: null } });
      await prisma.applicationForm.updateMany({ where: { id: { in: reviewIds } }, data: { reviewChannelId: null } });
    }
    if (panelIds.length) {
      await prisma.applicationForm.updateMany({ where: { id: { in: panelIds } }, data: { panelChannelId: null, panelMessageId: null } });
    }
  } catch (err) {
    logger.warn(`[applications] ${guildId}: channel cleanup failed: ${err?.message}`);
  }
}

/**
 * A role was deleted: it leaves the forms' accept and required lists. A form that required that role and has no
 * other required role left is switched off instead of silently opening to everyone.
 */
export async function onRoleDelete(ctx) {
  const id = ctx.role?.id;
  const guildId = ctx.guildId;
  if (!id || !guildId) return;
  try {
    const forms = await prisma.applicationForm.findMany({
      where: { guildId, OR: [{ acceptRoleIds: { has: id } }, { requiredRoleIds: { has: id } }] },
      select: { id: true, acceptRoleIds: true, requiredRoleIds: true },
    });
    for (const form of forms) {
      const required = form.requiredRoleIds.filter((r) => r !== id);
      const data = { acceptRoleIds: form.acceptRoleIds.filter((r) => r !== id), requiredRoleIds: required };
      if (form.requiredRoleIds.length > 0 && required.length === 0) data.enabled = false;
      await prisma.applicationForm.updateMany({ where: { id: form.id }, data });
    }
  } catch (err) {
    logger.warn(`[applications] ${guildId}: role cleanup failed: ${err?.message}`);
  }
}
