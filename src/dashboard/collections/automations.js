// The `automations` collection (Automations): MEE6-style When → If → Do rules, the Automation rows /automation writes.
// Every trigger, condition and action automationService supports, with the same limits, plus the /automation create
// presets; also the pure conversion between the stored condition/action JSON and the form's shape.
// automationService opens Redis when it is imported, so its tables and shape rules (TRIGGERS, CONDITION_TYPES,
// ACTION_TYPES, validateAutomationShape, the normalisers' limits) are restated here; test/dashboard-automations.test.js
// compares them with the service so the two cannot drift. Pure: nothing is read or connected at import time.
import { LIMITS } from '../../config/constants.js';
import { validateRegex, MAX_PATTERN_LENGTH } from '../../utils/safeRegex.js';
import { SLUG_MESSAGE, SLUG_PATTERN } from './embeds.js';
import { refKey } from '../schema.js';

// ── Restated from src/services/automationService.js (drift-tested) ───────────

/** Limits of the automation normalisers (automationService: CONTENT_MAX, THREAD_NAME_MAX, BUTTON_LABEL_MAX, ...). */
export const AUTOMATION_LIMITS = Object.freeze({
  name: 32, // AUTOMATION_NAME_MAX
  content: 2000, // CONTENT_MAX
  threadName: 100, // THREAD_NAME_MAX
  buttonLabel: 80, // BUTTON_LABEL_MAX
  buttonUrl: 512, // BUTTON_URL_MAX
  buttonIndex: 99, // MAX_BUTTON_INDEX
  cooldownSeconds: 604_800, // MAX_COOLDOWN_SECONDS
  rewardAmount: 1_000_000, // MAX_REWARD_AMOUNT
  idList: 25, // channelIds, categoryIds, roleIds, userIds and buttons of a condition
  word: 100, // CONTAINS / NOT_CONTAINS
  exactText: 500, // EXACT
  reactedEmojis: 10, // REACTED_WITH
  reactionsMin: 1000, // HAS_REACTIONS
  actionRoles: 10, // GIVE_ROLES / TAKE_ROLES
  threadUsers: 10, // ADD_TO_THREAD
});
export const THREAD_ARCHIVE_MINUTES = Object.freeze([60, 1440, 4320, 10080]);
export const DEFAULT_THREAD_NAME = "{user.name}'s thread";
export const WELCOMER_DEFAULT_TEXT = 'Hey {user}, welcome to **{server}**!';
export const DEFAULT_STAR = '⭐';
export const DEFAULT_THRESHOLD = 3;
export const BUTTON_STYLES = Object.freeze(['PRIMARY', 'SECONDARY', 'SUCCESS', 'DANGER']);
/** The texts automationService and /automation show (ERROR_TEXTS). */
export const NEEDS_ACTIONS = 'Add at least one action before turning it on.';
export const BUTTONS_USED = 'This automation has used all 99 button numbers. Create a new automation for more buttons.';

/** Trigger table (TRIGGERS); `channel`: the trigger happens in a channel (the default target of a message). */
export const TRIGGER_RULES = Object.freeze({
  MESSAGE_SENT: Object.freeze({ label: 'Message sent', channel: true, description: 'A member sends a message (bots and webhooks never trigger automations).' }),
  MESSAGE_DELETED: Object.freeze({ label: 'Message deleted', channel: true, description: "A member's message is deleted (deletions by the bot do not count)." }),
  MESSAGE_EDITED: Object.freeze({ label: 'Message edited', channel: true, description: 'A member edits a message.' }),
  BUTTON_CLICKED: Object.freeze({ label: 'Button clicked', channel: true, description: 'A member clicks a button that an automation posted.' }),
  REACTION_ADDED: Object.freeze({ label: 'Reaction added', channel: true, description: 'A member reacts to a message.' }),
  ROLE_GAINED: Object.freeze({ label: 'Role gained', channel: false, description: 'A member gets a role.' }),
  ROLE_LOST: Object.freeze({ label: 'Role lost', channel: false, description: 'A member loses a role.' }),
  VOICE_JOINED: Object.freeze({ label: 'Joined voice', channel: true, description: 'A member joins a voice channel (moving counts as leaving one and joining the other).' }),
  VOICE_LEFT: Object.freeze({ label: 'Left voice', channel: true, description: 'A member leaves a voice channel.' }),
  THREAD_CREATED: Object.freeze({ label: 'Thread created', channel: true, description: 'A member creates a thread.' }),
  MEMBER_JOINED: Object.freeze({ label: 'Member joined', channel: false, description: 'A new member joins the server (bots do not count).' }),
});

const ALL = Object.freeze(Object.keys(TRIGGER_RULES));
const LIVE = ['MESSAGE_SENT', 'MESSAGE_EDITED', 'REACTION_ADDED'];
/** The trigger groups of automationService (T_CHANNEL, T_CONTENT, ...). */
export const TRIGGER_SETS = Object.freeze({
  ALL,
  CHANNEL: Object.freeze(['MESSAGE_SENT', 'MESSAGE_DELETED', 'MESSAGE_EDITED', 'BUTTON_CLICKED', 'REACTION_ADDED', 'VOICE_JOINED', 'VOICE_LEFT', 'THREAD_CREATED']),
  CONTENT: Object.freeze(['MESSAGE_SENT', 'MESSAGE_DELETED', 'MESSAGE_EDITED', 'REACTION_ADDED']),
  LIVE: Object.freeze(LIVE), // the member's message still exists
  COUNTS: Object.freeze(['MESSAGE_DELETED', 'MESSAGE_EDITED', 'REACTION_ADDED']), // reaction counts mean something
  THREADY: Object.freeze(['THREAD_CREATED', 'MESSAGE_SENT', 'MESSAGE_EDITED', 'REACTION_ADDED', 'BUTTON_CLICKED']),
  REPLYABLE: Object.freeze([...LIVE, 'BUTTON_CLICKED']),
  REACTION: Object.freeze(['REACTION_ADDED']),
  ROLE_GAINED: Object.freeze(['ROLE_GAINED']),
  ROLE_LOST: Object.freeze(['ROLE_LOST']),
  BUTTON: Object.freeze(['BUTTON_CLICKED']),
});
const S = TRIGGER_SETS;

const cond = (label, triggers, extra = {}) => Object.freeze({ label, triggers, ...extra });
/** Condition table (CONDITION_TYPES): `field` is the form field holding the condition's parameters. */
export const CONDITION_RULES = Object.freeze({
  CHANNEL_IN: cond('Channel is one of', S.CHANNEL, { field: 'channelIds' }),
  CHANNEL_NOT_IN: cond('Channel is not one of', S.CHANNEL, { field: 'channelIds' }),
  CATEGORY_IN: cond('Category is one of', S.CHANNEL, { field: 'categoryIds' }),
  CATEGORY_NOT_IN: cond('Category is not one of', S.CHANNEL, { field: 'categoryIds' }),
  CONTAINS: cond('Message contains words', S.CONTENT, { field: 'words' }),
  EXACT: cond('Message is exactly', S.CONTENT, { field: 'texts' }),
  NOT_CONTAINS: cond('Message does not contain words', S.CONTENT, { field: 'words' }),
  MATCHES_REGEX: cond('Message matches a regex', S.CONTENT, { field: 'pattern' }),
  ATTACHMENT_AUDIO: cond('Has an audio attachment', S.CONTENT, { noParams: true }),
  ATTACHMENT_IMAGE: cond('Has an image attachment', S.CONTENT, { noParams: true }),
  ATTACHMENT_VIDEO: cond('Has a video attachment', S.CONTENT, { noParams: true }),
  ATTACHMENT_TEXT: cond('Has a text-file attachment', S.CONTENT, { noParams: true }),
  REACTED_WITH: cond('Reacted with emoji', S.REACTION, { field: 'emojis' }),
  HAS_REACTIONS: cond('Message has at least N reactions', S.COUNTS, { field: 'min' }),
  IS_REPLY: cond('Message is a reply', S.CONTENT, { noParams: true }),
  IS_NOT_REPLY: cond('Message is not a reply', S.CONTENT, { noParams: true }),
  HAS_ANY_ROLE: cond('Member has any of these roles', S.ALL, { field: 'roleIds' }),
  HAS_ALL_ROLES: cond('Member has all of these roles', S.ALL, { field: 'roleIds' }),
  HAS_NOT_ANY_ROLE: cond('Member has none of these roles', S.ALL, { field: 'roleIds' }),
  HAS_NOT_ALL_ROLES: cond('Member lacks one of these roles', S.ALL, { field: 'roleIds' }),
  GAINED_ROLE: cond('Gained one of these roles', S.ROLE_GAINED, { field: 'roleIds' }),
  LOST_ROLE: cond('Lost one of these roles', S.ROLE_LOST, { field: 'roleIds' }),
  USER_IS: cond('User is one of', S.ALL, { field: 'users' }),
  BUTTON_IS: cond('Clicked this button', S.BUTTON, { field: 'buttons' }),
});

const act = (label, triggers, extra = {}) => Object.freeze({ label, triggers, ...extra });
/**
 * Action table (ACTION_TYPES). `triggers` is the base applicability; SEND_MESSAGE / SEND_EMBED need a channel outside
 * TRIGGER_SETS.CHANNEL, CREATE_THREAD needs one outside TRIGGER_SETS.LIVE, and the thread actions need a THREADY trigger
 * or an earlier "Create a thread" (shapeProblems).
 */
export const ACTION_RULES = Object.freeze({
  CREATE_THREAD: act('Create a thread', S.ALL),
  SEND_MESSAGE: act('Send a message', S.ALL, { button: true }),
  REPLY: act('Reply', S.REPLYABLE, { button: true }),
  REPOST: act('Repost the message', S.CONTENT),
  PIN: act('Pin the message', S.LIVE),
  DELETE: act('Delete the message', S.LIVE),
  REACT: act('React with an emoji', S.LIVE),
  REMOVE_REACTION: act('Remove the reaction', S.REACTION),
  GIVE_ROLES: act('Give roles', S.ALL),
  TAKE_ROLES: act('Take roles', S.ALL),
  ADD_TO_THREAD: act('Add members to the thread', S.ALL, { thread: true }),
  SEND_IN_THREAD: act('Send a message in the thread', S.ALL, { button: true, thread: true }),
  GIVE_COINS: act('Give coins', S.ALL, { reward: 'coins' }),
  TAKE_COINS: act('Take coins', S.ALL, { reward: 'coins' }),
  GIVE_XP: act('Give XP', S.ALL, { reward: 'xp' }),
  TAKE_XP: act('Take XP', S.ALL, { reward: 'xp' }),
  SEND_EMBED: act('Send a saved embed', S.ALL),
});

/** /automation create presets (PRESETS) and the options each one reads (PRESET_OPTIONS, in form field names). */
export const PRESETS = Object.freeze(['welcomer', 'starboard', 'thread-joiner']);
export const PRESET_FIELDS = Object.freeze({
  welcomer: Object.freeze(['presetChannelId', 'presetMessage', 'presetRoleId']),
  starboard: Object.freeze(['presetChannelId', 'presetEmoji', 'presetThreshold']),
  'thread-joiner': Object.freeze(['presetUserId', 'presetChannelId']),
});

/** Variables automation texts support besides the built-in ones (automationService.buildVars). */
export const AUTOMATION_VARIABLES = Object.freeze([
  '{automation}', '{message.content}', '{message.link}', '{message.id}', '{message.author}', '{message.author.name}',
  '{message.old_content}', '{reaction}', '{role}', '{role.id}', '{role.name}', '{voice}', '{voice.name}', '{thread}',
  '{thread.name}',
]);

/**
 * The automationId a create stores for a "Clicked this button" condition that points at the new automation itself; the
 * store passes it as createAutomation's `selfRefId`, which rewrites it to the new row's id (automationService's
 * AUTOMATION_ID_RE: 8 to 40 letters and digits; a cuid never equals it).
 */
export const SELF_REF_ID = 'dashboardselfreference';

/** The Redis key of an automation's last runtime problem (automationService problemKey; CLAUDE.md Redis table). */
export const problemKey = (automationId) => `automation:problem:${automationId}`;

// ── Form descriptors ─────────────────────────────────────────────────────────

const labelsOf = (triggers) => triggers.map((t) => TRIGGER_RULES[t].label);
const worksWith = (triggers) => (triggers.length === ALL.length ? '' : ` Works with: ${labelsOf(triggers).join(', ')}.`);
const typesWhere = (table, pred) => Object.entries(table).filter(([, d]) => pred(d)).map(([t]) => t);
const showFor = (types) => ({ field: 'type', in: [...types] });

const CONDITION_HINTS = Object.freeze({
  CHANNEL_IN: 'It happened in one of these channels (or in a thread of one).',
  CHANNEL_NOT_IN: 'It happened outside these channels.',
  CATEGORY_IN: 'It happened in a channel of one of these categories.',
  CATEGORY_NOT_IN: 'It happened outside these categories.',
  CONTAINS: 'The message contains at least one of these words (whole words, any case).',
  EXACT: 'The whole message is one of these texts (any case).',
  NOT_CONTAINS: 'The message contains none of these words.',
  MATCHES_REGEX: 'The message matches a regular expression.',
  ATTACHMENT_AUDIO: 'The message has an audio file attached.',
  ATTACHMENT_IMAGE: 'The message has an image attached.',
  ATTACHMENT_VIDEO: 'The message has a video attached.',
  ATTACHMENT_TEXT: 'The message has a text file attached (.txt, .md, .log, .csv, .json).',
  REACTED_WITH: 'The reaction is one of these emojis.',
  HAS_REACTIONS: 'The message has at least this many reactions.',
  IS_REPLY: 'The message replies to another message.',
  IS_NOT_REPLY: 'The message is not a reply.',
  HAS_ANY_ROLE: 'The member has at least one of these roles.',
  HAS_ALL_ROLES: 'The member has every one of these roles.',
  HAS_NOT_ANY_ROLE: 'The member has none of these roles.',
  HAS_NOT_ALL_ROLES: 'The member is missing at least one of these roles.',
  GAINED_ROLE: 'The role the member got is one of these.',
  LOST_ROLE: 'The role the member lost is one of these.',
  USER_IS: 'The member is one of these people.',
  BUTTON_IS: 'The clicked button is one of these automation buttons.',
});

const ACTION_HINTS = Object.freeze({
  CREATE_THREAD: 'Starts a thread on the message, or in a channel you pick.',
  SEND_MESSAGE: 'Posts a message where it happened or in a channel you pick.',
  REPLY: 'Replies to the message (or to the button click, visible to everyone).',
  REPOST: 'Copies the message into another channel, once per message.',
  PIN: 'Pins the message.',
  DELETE: 'Deletes the message.',
  REACT: 'Adds a reaction to the message.',
  REMOVE_REACTION: 'Removes the reaction that triggered it, or one emoji you pick.',
  GIVE_ROLES: 'Gives the member these roles.',
  TAKE_ROLES: 'Takes these roles from the member.',
  ADD_TO_THREAD: 'Adds people to the thread (the member who triggered it when empty).',
  SEND_IN_THREAD: 'Posts a message in the thread.',
  GIVE_COINS: 'Gives the member coins (needs the Economy module).',
  TAKE_COINS: 'Takes coins from the member (needs the Economy module).',
  GIVE_XP: 'Gives the member XP (needs the Levels module).',
  TAKE_XP: 'Takes XP from the member (needs the Levels module).',
  SEND_EMBED: 'Posts one of your saved embeds.',
});

const CHANNEL_CONDITIONS = ['CHANNEL_IN', 'CHANNEL_NOT_IN'];
const CATEGORY_CONDITIONS = ['CATEGORY_IN', 'CATEGORY_NOT_IN'];
const WORD_CONDITIONS = ['CONTAINS', 'NOT_CONTAINS'];
const ROLE_CONDITIONS = ['HAS_ANY_ROLE', 'HAS_ALL_ROLES', 'HAS_NOT_ANY_ROLE', 'HAS_NOT_ALL_ROLES', 'GAINED_ROLE', 'LOST_ROLE'];
const MESSAGE_ACTIONS = typesWhere(ACTION_RULES, (d) => d.button); // SEND_MESSAGE, REPLY, SEND_IN_THREAD
const TARGET_ACTIONS = ['SEND_MESSAGE', 'SEND_EMBED', 'REPOST'];
const REWARD_ACTIONS = typesWhere(ACTION_RULES, (d) => d.reward);

/** Channel kinds a condition's channel list takes (any channel but a category; threads are not offered). */
const CONDITION_CHANNEL_KINDS = Object.freeze(['text', 'announcement', 'voice', 'stage', 'forum', 'media']);
/** Where an action posts (the /automation action add channel option: text, announcement, voice and stage chats). */
const TARGET_CHANNEL_KINDS = Object.freeze(['text', 'announcement', 'voice', 'stage']);
/** Where "Create a thread" may start one (a text or announcement channel). */
const THREAD_CHANNEL_KINDS = Object.freeze(['text', 'announcement']);
/** The /automation create channel option (Thread Joiner may watch forums and media channels too). */
const PRESET_CHANNEL_KINDS = Object.freeze(['text', 'announcement', 'forum', 'media']);

const REGEX_FLAGS = Object.freeze([
  { value: 'i', label: 'Ignore case' },
  { value: 'm', label: '^ and $ match at every line' },
  { value: 's', label: '. also matches line breaks' },
  { value: 'u', label: 'Unicode' },
]);

const CONDITION_FIELDS = Object.freeze([
  {
    field: 'type',
    type: 'enum',
    label: 'Condition',
    options: Object.entries(CONDITION_RULES).map(([value, d]) => ({
      value, label: d.label, description: `${CONDITION_HINTS[value]}${worksWith(d.triggers)}`,
    })),
    default: 'CHANNEL_IN',
  },
  {
    field: 'channelIds',
    type: 'channels',
    label: 'Channels',
    description: 'Threads count as their parent channel.',
    channelTypes: [...CONDITION_CHANNEL_KINDS],
    maxItems: AUTOMATION_LIMITS.idList,
    default: [],
    showWhen: showFor(CHANNEL_CONDITIONS),
  },
  {
    field: 'categoryIds',
    type: 'channels',
    label: 'Categories',
    channelTypes: ['category'],
    maxItems: AUTOMATION_LIMITS.idList,
    default: [],
    showWhen: showFor(CATEGORY_CONDITIONS),
  },
  {
    field: 'words',
    type: 'stringList',
    label: 'Words',
    description: 'Whole words or phrases, any case.',
    maxItems: AUTOMATION_LIMITS.idList,
    itemMaxLength: AUTOMATION_LIMITS.word,
    default: [],
    showWhen: showFor(WORD_CONDITIONS),
  },
  {
    field: 'texts',
    type: 'stringList',
    label: 'Texts',
    description: 'The whole message must be one of these (any case).',
    maxItems: AUTOMATION_LIMITS.idList,
    itemMaxLength: AUTOMATION_LIMITS.exactText,
    default: [],
    showWhen: showFor(['EXACT']),
  },
  {
    field: 'pattern',
    type: 'text',
    label: 'Regular expression',
    description: `Without slashes, e.g. ^hello\\b. Up to ${MAX_PATTERN_LENGTH} characters; patterns that could hang (such as (a+)+) are refused.`,
    multiline: true, // never trimmed: spaces at either end are part of the pattern
    rows: 1,
    maxLength: MAX_PATTERN_LENGTH,
    nullable: true,
    default: null,
    showWhen: showFor(['MATCHES_REGEX']),
  },
  {
    field: 'flags',
    type: 'multiEnum',
    label: 'Regex options',
    options: REGEX_FLAGS.map((o) => ({ ...o })),
    default: ['i'],
    showWhen: showFor(['MATCHES_REGEX']),
  },
  {
    field: 'emojis',
    type: 'objectList',
    label: 'Emojis',
    itemLabel: 'Emoji',
    addLabel: 'Add emoji',
    maxItems: AUTOMATION_LIMITS.reactedEmojis,
    itemFields: [{ field: 'emoji', type: 'emoji', label: 'Emoji', required: true }],
    default: [],
    showWhen: showFor(['REACTED_WITH']),
  },
  {
    field: 'min',
    type: 'int',
    label: 'At least',
    min: 1,
    max: AUTOMATION_LIMITS.reactionsMin,
    unit: 'reactions',
    default: DEFAULT_THRESHOLD,
    showWhen: showFor(['HAS_REACTIONS']),
  },
  {
    field: 'emoji',
    type: 'emoji',
    label: 'Only count this emoji',
    description: 'Empty: every reaction counts.',
    nullable: true,
    default: null,
    showWhen: showFor(['HAS_REACTIONS']),
  },
  {
    field: 'roleIds',
    type: 'roles',
    label: 'Roles',
    maxItems: AUTOMATION_LIMITS.idList,
    default: [],
    showWhen: showFor(ROLE_CONDITIONS),
  },
  {
    field: 'users',
    type: 'objectList',
    label: 'Members',
    itemLabel: 'Member',
    addLabel: 'Add member',
    maxItems: AUTOMATION_LIMITS.idList,
    itemFields: [{ field: 'userId', type: 'user', label: 'Member', required: true }],
    default: [],
    showWhen: showFor(['USER_IS']),
  },
  {
    field: 'buttons',
    type: 'objectList',
    label: 'Buttons',
    description: 'Automation buttons are posted by "Send a message", "Reply" or "Send a message in the thread" actions; '
      + 'each has a number shown on that action.',
    itemLabel: 'Button',
    addLabel: 'Add button',
    maxItems: AUTOMATION_LIMITS.idList,
    itemFields: [
      {
        field: 'automation',
        type: 'text',
        label: 'Automation',
        description: 'The name of the automation that posts the button (it may be this one).',
        maxLength: 40,
        minLength: 1,
        pattern: '[A-Za-z0-9_-]{1,40}',
        patternMessage: 'Type the name of an automation (letters, numbers, - and _).',
        required: true,
      },
      { field: 'index', type: 'int', label: 'Button number', min: 1, max: AUTOMATION_LIMITS.buttonIndex, default: 1, required: true },
    ],
    default: [],
    showWhen: showFor(['BUTTON_IS']),
  },
]);

const BUTTON_FIELDS = Object.freeze([
  {
    field: 'kind',
    type: 'enum',
    label: 'Button',
    options: [
      { value: 'none', label: 'No button' },
      { value: 'link', label: 'Link', description: 'Opens a web page.' },
      { value: 'automation', label: 'Automation button', description: 'Clicking it runs your "Button clicked" automations.' },
    ],
    default: 'none',
  },
  {
    field: 'label',
    type: 'text',
    label: 'Button label',
    maxLength: AUTOMATION_LIMITS.buttonLabel,
    nullable: true,
    default: null,
    showWhen: { field: 'kind', in: ['link', 'automation'] },
  },
  {
    field: 'url',
    type: 'url',
    label: 'Link',
    maxLength: AUTOMATION_LIMITS.buttonUrl,
    nullable: true,
    default: null,
    showWhen: { field: 'kind', in: ['link'] },
  },
  {
    field: 'style',
    type: 'enum',
    label: 'Colour',
    options: [
      { value: 'PRIMARY', label: 'Blurple' },
      { value: 'SECONDARY', label: 'Grey' },
      { value: 'SUCCESS', label: 'Green' },
      { value: 'DANGER', label: 'Red' },
    ],
    default: 'PRIMARY',
    showWhen: { field: 'kind', in: ['automation'] },
  },
  {
    field: 'index',
    type: 'int',
    label: 'Button number',
    description: 'Given when you save (leave it empty for a new button). "Clicked this button" conditions use it.',
    min: 1,
    max: AUTOMATION_LIMITS.buttonIndex,
    nullable: true,
    default: null,
    showWhen: { field: 'kind', in: ['automation'] },
  },
]);

const ACTION_FIELDS = Object.freeze([
  {
    field: 'type',
    type: 'enum',
    label: 'Action',
    options: Object.entries(ACTION_RULES).map(([value, d]) => ({
      value, label: d.label, description: `${ACTION_HINTS[value]}${worksWith(d.triggers)}`,
    })),
    default: 'SEND_MESSAGE',
  },
  {
    field: 'content',
    type: 'text',
    label: 'Message',
    description: 'Up to 2,000 characters. Variables such as {user} and {message.link} are filled in.',
    multiline: true,
    maxLength: AUTOMATION_LIMITS.content,
    nullable: true,
    placeholders: [...AUTOMATION_VARIABLES],
    default: null,
    showWhen: showFor(MESSAGE_ACTIONS),
  },
  {
    field: 'button',
    type: 'object',
    label: 'Button',
    description: 'Optional: one link or automation button under the message.',
    fields: BUTTON_FIELDS,
    default: { kind: 'none', label: null, url: null, style: 'PRIMARY', index: null },
    showWhen: showFor(MESSAGE_ACTIONS),
  },
  {
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: 'Empty: the channel where it happened (pick one for triggers that happen outside a channel).',
    channelTypes: [...TARGET_CHANNEL_KINDS],
    nullable: true,
    default: null,
    showWhen: showFor(TARGET_ACTIONS),
  },
  {
    field: 'threadName',
    type: 'text',
    label: 'Thread name',
    description: 'Up to 100 characters; variables are filled in.',
    maxLength: AUTOMATION_LIMITS.threadName,
    nullable: true,
    placeholders: [...AUTOMATION_VARIABLES],
    default: DEFAULT_THREAD_NAME,
    showWhen: showFor(['CREATE_THREAD']),
  },
  {
    field: 'threadChannelId',
    type: 'channel',
    label: 'Start it in',
    description: 'Empty: a thread on the member\'s message. Pick a channel to start a new thread there instead.',
    channelTypes: [...THREAD_CHANNEL_KINDS],
    nullable: true,
    default: null,
    showWhen: showFor(['CREATE_THREAD']),
  },
  {
    field: 'autoArchiveMinutes',
    type: 'enum',
    label: 'Hide after inactivity',
    options: [
      { value: 60, label: '1 hour' },
      { value: 1440, label: '1 day' },
      { value: 4320, label: '3 days' },
      { value: 10080, label: '1 week' },
    ],
    default: 1440,
    showWhen: showFor(['CREATE_THREAD']),
  },
  {
    field: 'emoji',
    type: 'emoji',
    label: 'Emoji',
    description: '"Remove the reaction": empty removes the reaction that triggered it.',
    nullable: true,
    default: null,
    showWhen: showFor(['REACT', 'REMOVE_REACTION']),
  },
  {
    field: 'roleIds',
    type: 'roles',
    label: 'Roles',
    description: 'Roles below the bot\'s highest role, without moderator permissions.',
    assignable: true,
    maxItems: AUTOMATION_LIMITS.actionRoles,
    default: [],
    showWhen: showFor(['GIVE_ROLES', 'TAKE_ROLES']),
  },
  {
    field: 'users',
    type: 'objectList',
    label: 'Members to add',
    description: 'Empty: the member who triggered it.',
    itemLabel: 'Member',
    addLabel: 'Add member',
    maxItems: AUTOMATION_LIMITS.threadUsers,
    itemFields: [{ field: 'userId', type: 'user', label: 'Member', required: true }],
    default: [],
    showWhen: showFor(['ADD_TO_THREAD']),
  },
  {
    field: 'amount',
    type: 'int',
    label: 'Amount',
    min: 1,
    max: AUTOMATION_LIMITS.rewardAmount,
    default: 100,
    showWhen: showFor(REWARD_ACTIONS),
  },
  {
    field: 'embedName',
    type: 'text',
    label: 'Saved embed',
    description: 'The name of one of your saved embeds (Embed Messages).',
    maxLength: AUTOMATION_LIMITS.name,
    pattern: SLUG_PATTERN,
    patternMessage: 'Type the name of a saved embed (lowercase letters, numbers, - and _).',
    nullable: true,
    default: null,
    showWhen: showFor(['SEND_EMBED']),
  },
]);

const PRESET_SHOWN = (presets) => ({ field: 'preset', in: [...presets] });
const BUILDER_SHOWN = { field: 'preset', in: ['none'] };

export const collection = Object.freeze({
  key: 'automations',
  module: 'automations',
  label: 'Automations',
  singular: 'automation',
  description: 'When → If → Do rules: when something happens (a message, a reaction, a role change, a member joining, a '
    + 'button click…), check your conditions and run actions such as sending a message, giving roles, XP or coins. '
    + 'Shared with /automation in Discord, where you can also export and import them as JSON. The bot\'s permissions '
    + 'in the channels you pick are checked when it runs; "Check for problems" lists what went wrong.',
  limit: LIMITS.AUTOMATIONS,
  titleField: 'name',
  subtitleField: 'summary',
  fields: Object.freeze([
    {
      field: 'name',
      type: 'text',
      label: 'Name',
      description: 'Used by /automation in Discord. Renaming keeps its buttons working.',
      maxLength: AUTOMATION_LIMITS.name,
      minLength: 1,
      pattern: SLUG_PATTERN,
      patternMessage: SLUG_MESSAGE,
      required: true,
    },
    {
      field: 'preset',
      type: 'enum',
      label: 'Preset',
      description: 'Start from a ready-made recipe, or build your own.',
      options: [
        { value: 'none', label: 'Build my own' },
        { value: 'welcomer', label: 'Welcomer', description: 'When a member joins: post a welcome message in a channel, and optionally give a role.' },
        { value: 'starboard', label: 'Starboard', description: 'When a message gets enough reactions with an emoji: repost it in a channel.' },
        { value: 'thread-joiner', label: 'Thread Joiner', description: 'When a thread is created (optionally only in one channel): add a member to it.' },
      ],
      default: 'none',
    },
    {
      field: 'presetChannelId',
      type: 'channel',
      label: 'Channel',
      description: 'Welcomer: where the welcome message is posted. Starboard: where popular messages are reposted (both: a '
        + 'text or announcement channel). Thread Joiner: only threads created in this channel (empty: every channel).',
      channelTypes: [...PRESET_CHANNEL_KINDS],
      nullable: true,
      default: null,
      showWhen: PRESET_SHOWN(PRESETS),
    },
    {
      field: 'presetMessage',
      type: 'text',
      label: 'Welcome message',
      description: `Empty: "${WELCOMER_DEFAULT_TEXT}"`,
      multiline: true,
      maxLength: AUTOMATION_LIMITS.content,
      nullable: true,
      placeholders: [...AUTOMATION_VARIABLES],
      default: null,
      showWhen: PRESET_SHOWN(['welcomer']),
    },
    {
      field: 'presetRoleId',
      type: 'role',
      label: 'Role to give',
      description: 'Optional. Below the bot\'s highest role, without moderator permissions.',
      assignable: true,
      nullable: true,
      default: null,
      showWhen: PRESET_SHOWN(['welcomer']),
    },
    {
      field: 'presetEmoji',
      type: 'emoji',
      label: 'Emoji',
      description: `Empty: ${DEFAULT_STAR}`,
      nullable: true,
      default: null,
      showWhen: PRESET_SHOWN(['starboard']),
    },
    {
      field: 'presetThreshold',
      type: 'int',
      label: 'Reactions needed',
      min: 1,
      max: AUTOMATION_LIMITS.reactionsMin,
      unit: 'reactions',
      default: DEFAULT_THRESHOLD,
      showWhen: PRESET_SHOWN(['starboard']),
    },
    {
      field: 'presetUserId',
      type: 'user',
      label: 'Member to add',
      description: 'Added to every new thread (a person, not a bot).',
      nullable: true,
      default: null,
      showWhen: PRESET_SHOWN(['thread-joiner']),
    },
    {
      field: 'trigger',
      type: 'enum',
      label: 'When',
      description: 'What starts it. Conditions and actions that do not fit the trigger are pointed out when you save.',
      options: Object.entries(TRIGGER_RULES).map(([value, d]) => ({ value, label: d.label, description: d.description })),
      default: 'MESSAGE_SENT',
      showWhen: BUILDER_SHOWN,
    },
    {
      field: 'conditions',
      type: 'objectList',
      label: 'If',
      description: `Every condition must pass (up to ${LIMITS.AUTOMATION_CONDITIONS}).`,
      itemLabel: 'Condition',
      addLabel: 'Add condition',
      emptyText: 'No conditions: it runs every time.',
      itemFields: CONDITION_FIELDS,
      maxItems: LIMITS.AUTOMATION_CONDITIONS,
      default: [],
      showWhen: BUILDER_SHOWN,
    },
    {
      field: 'actions',
      type: 'objectList',
      label: 'Do',
      description: `Run in order (up to ${LIMITS.AUTOMATION_ACTIONS}).`,
      itemLabel: 'Action',
      addLabel: 'Add action',
      emptyText: 'No actions yet. Add at least one before turning it on.',
      itemFields: ACTION_FIELDS,
      maxItems: LIMITS.AUTOMATION_ACTIONS,
      default: [],
      showWhen: BUILDER_SHOWN,
    },
    {
      field: 'cooldownSeconds',
      type: 'int',
      label: 'Cooldown per member',
      description: 'How long before the same member can trigger it again (0 = no cooldown, up to 7 days).',
      min: 0,
      max: AUTOMATION_LIMITS.cooldownSeconds,
      unit: 'seconds',
      default: 0,
    },
    { field: 'enabled', type: 'boolean', label: 'Enabled', description: 'Off: it never runs.', default: true },
  ]),
  createOnly: Object.freeze([
    'preset', 'presetChannelId', 'presetMessage', 'presetRoleId', 'presetEmoji', 'presetThreshold', 'presetUserId',
  ]),
  readOnly: Object.freeze([
    { field: 'summary', type: 'text', label: 'Summary', maxLength: 200, default: '' },
    { field: 'uses', type: 'int', label: 'Runs', min: 0, max: 2_147_483_647, default: 0 },
    {
      field: 'problems', type: 'stringList', label: 'Problems', maxItems: 100, itemMaxLength: 400, default: [],
    },
    // The creator's Discord name when the dashboard could look it up, else their user id.
    { field: 'createdBy', type: 'text', label: 'Created by', maxLength: 100, nullable: true, default: null },
    { field: 'createdAt', type: 'datetime', label: 'Created', nullable: true, default: null },
    { field: 'updatedAt', type: 'datetime', label: 'Last edited', nullable: true, default: null },
  ]),
  actions: Object.freeze([
    {
      id: 'review',
      label: 'Check for problems',
      description: 'Looks for deleted channels and roles, roles the bot cannot give, missing saved embeds, reward modules '
        + 'that are off, broken button links and the last error the bot ran into.',
      fields: [],
    },
  ]),
  check: checkAutomation,
  // An action's `roleIds` are given or taken depending on its type, so the server's "may this admin give that role"
  // checks key them by type: a role kept in a Take action and turned into a Give action is checked like a new one
  // (server.js verifyRefs). Server-only, never sent to the browser.
  refKeys: actionRoleKeys,
  refKeyOf: actionRoleKeyOf,
});

const ACTION_ROLE_PATH_RE = /^actions\.(\d+)\.roleIds\.\d+$/;
const actionRoleKey = (type, id) => `actions.${String(type)}.roleIds|${id}`;

/** The stored keys of an automation item: the generic ones, with action roles keyed by their action's type. */
function actionRoleKeys(item, keys) {
  const out = new Set([...keys].filter((k) => !k.startsWith('actions.roleIds|')));
  for (const action of Array.isArray(item?.actions) ? item.actions : []) {
    for (const id of Array.isArray(action?.roleIds) ? action.roleIds : []) out.add(actionRoleKey(action.type, id));
  }
  return out;
}

/** The key of a role in a request: an action role by the type that action has in the request. */
function actionRoleKeyOf(ref, data) {
  const m = ACTION_ROLE_PATH_RE.exec(ref.path);
  if (!m) return refKey(ref.path, ref.id);
  return actionRoleKey(data?.actions?.[Number(m[1])]?.type, ref.id);
}

// ── Form <-> stored shapes ───────────────────────────────────────────────────

const asList = (v) => (Array.isArray(v) ? v : []);
const nonEmpty = (v) => typeof v === 'string' && v.trim() !== '';

/** A condition item with every field at its default (the hidden fields of other types keep these). */
export function blankCondition(type = 'CHANNEL_IN') {
  return {
    type, channelIds: [], categoryIds: [], words: [], texts: [], pattern: null, flags: ['i'], emojis: [],
    min: DEFAULT_THRESHOLD, emoji: null, roleIds: [], users: [], buttons: [],
  };
}

/** An action item with every field at its default. */
export function blankAction(type = 'SEND_MESSAGE') {
  return {
    type,
    content: null,
    button: { kind: 'none', label: null, url: null, style: 'PRIMARY', index: null },
    channelId: null,
    threadName: DEFAULT_THREAD_NAME,
    threadChannelId: null,
    autoArchiveMinutes: 1440,
    emoji: null,
    roleIds: [],
    users: [],
    amount: 100,
    embedName: null,
  };
}

const FLAG_ORDER = REGEX_FLAGS.map((f) => f.value);
const flagsToForm = (flags) => FLAG_ORDER.filter((f) => String(flags ?? '').includes(f));
/** The stored flags string: unique and sorted, as automationService's normalizeFlags keeps it. */
export const flagsToStored = (flags) => [...new Set(asList(flags))].filter((f) => FLAG_ORDER.includes(f)).sort().join('');

/**
 * One stored condition (automationService.normalizeCondition shape) in the form's shape.
 * @param {object} c
 * @param {(automationId: string) => string|null} [nameOf] the automation name of an id (BUTTON_IS); unknown ids stay ids
 */
export function conditionToForm(c, nameOf = () => null) {
  const f = blankCondition(c?.type);
  switch (c?.type) {
    case 'CHANNEL_IN': case 'CHANNEL_NOT_IN': f.channelIds = [...asList(c.channelIds)]; break;
    case 'CATEGORY_IN': case 'CATEGORY_NOT_IN': f.categoryIds = [...asList(c.categoryIds)]; break;
    case 'CONTAINS': case 'NOT_CONTAINS': f.words = [...asList(c.words)]; break;
    case 'EXACT': f.texts = [...asList(c.texts)]; break;
    case 'MATCHES_REGEX': f.pattern = c.pattern ?? null; f.flags = flagsToForm(c.flags); break;
    case 'REACTED_WITH': f.emojis = asList(c.emojis).map((emoji) => ({ emoji })); break;
    case 'HAS_REACTIONS': f.min = c.min; f.emoji = c.emoji ?? null; break;
    case 'USER_IS': f.users = asList(c.userIds).map((userId) => ({ userId })); break;
    case 'BUTTON_IS':
      f.buttons = asList(c.buttons).map((b) => ({ automation: nameOf(b.automationId) ?? b.automationId, index: b.index }));
      break;
    default:
      if (ROLE_CONDITIONS.includes(c?.type)) f.roleIds = [...asList(c.roleIds)];
  }
  return f;
}

/**
 * One form condition (validated) in the stored shape; parameters of other types are dropped. The result still goes
 * through automationService.normalizeCondition before it is stored.
 * @param {object} f
 * @param {(nameOrId: string) => string} [automationIdOf] BUTTON_IS: the automation id a typed name stands for
 */
export function conditionToStored(f, automationIdOf = (x) => x) {
  const { type } = f;
  switch (type) {
    case 'CHANNEL_IN': case 'CHANNEL_NOT_IN': return { type, channelIds: [...asList(f.channelIds)] };
    case 'CATEGORY_IN': case 'CATEGORY_NOT_IN': return { type, categoryIds: [...asList(f.categoryIds)] };
    case 'CONTAINS': case 'NOT_CONTAINS': return { type, words: [...asList(f.words)] };
    case 'EXACT': return { type, texts: [...asList(f.texts)] };
    case 'MATCHES_REGEX': return { type, pattern: f.pattern ?? '', flags: flagsToStored(f.flags) };
    case 'REACTED_WITH': return { type, emojis: asList(f.emojis).map((e) => e.emoji) };
    case 'HAS_REACTIONS': return { type, min: f.min, emoji: f.emoji ?? null };
    case 'USER_IS': return { type, userIds: asList(f.users).map((u) => u.userId) };
    case 'BUTTON_IS': return { type, buttons: asList(f.buttons).map((b) => ({ automationId: automationIdOf(b.automation), index: b.index })) };
    default:
      return ROLE_CONDITIONS.includes(type) ? { type, roleIds: [...asList(f.roleIds)] } : { type };
  }
}

function buttonToForm(b) {
  if (!b) return { kind: 'none', label: null, url: null, style: 'PRIMARY', index: null };
  if (b.url != null) return { kind: 'link', label: b.label, url: b.url, style: 'PRIMARY', index: null };
  return { kind: 'automation', label: b.label, url: null, style: b.style ?? 'PRIMARY', index: b.index ?? null };
}

function buttonToStored(b, index) {
  if (b?.kind === 'link') return { label: b.label, url: b.url };
  if (b?.kind === 'automation') return { label: b.label, url: null, style: b.style ?? 'PRIMARY', index: index ?? b.index ?? null };
  return null;
}

/** One stored action (automationService.normalizeAction shape) in the form's shape. */
export function actionToForm(a) {
  const f = blankAction(a?.type);
  switch (a?.type) {
    case 'CREATE_THREAD':
      f.threadName = a.name ?? DEFAULT_THREAD_NAME;
      f.threadChannelId = a.channelId ?? null;
      f.autoArchiveMinutes = a.autoArchiveMinutes ?? 1440;
      break;
    case 'SEND_MESSAGE': f.content = a.content; f.channelId = a.channelId ?? null; f.button = buttonToForm(a.button); break;
    case 'REPLY': case 'SEND_IN_THREAD': f.content = a.content; f.button = buttonToForm(a.button); break;
    case 'REPOST': f.channelId = a.channelId ?? null; break;
    case 'REACT': case 'REMOVE_REACTION': f.emoji = a.emoji ?? null; break;
    case 'GIVE_ROLES': case 'TAKE_ROLES': f.roleIds = [...asList(a.roleIds)]; break;
    case 'ADD_TO_THREAD': f.users = asList(a.userIds).map((userId) => ({ userId })); break;
    case 'SEND_EMBED': f.embedName = a.embedName; f.channelId = a.channelId ?? null; break;
    default:
      if (REWARD_ACTIONS.includes(a?.type)) f.amount = a.amount;
  }
  return f;
}

/**
 * One form action (validated) in the stored shape; options of other types are dropped. The result still goes through
 * automationService.normalizeAction before it is stored.
 * @param {object} f
 * @param {number|null} [buttonIndex] the number the automation button gets (planButtons); default: the form's
 */
export function actionToStored(f, buttonIndex = null) {
  const { type } = f;
  switch (type) {
    case 'CREATE_THREAD':
      return { type, name: nonEmpty(f.threadName) ? f.threadName : null, channelId: f.threadChannelId ?? null, autoArchiveMinutes: f.autoArchiveMinutes };
    case 'SEND_MESSAGE': return { type, content: f.content ?? '', channelId: f.channelId ?? null, button: buttonToStored(f.button, buttonIndex) };
    case 'REPLY': case 'SEND_IN_THREAD': return { type, content: f.content ?? '', button: buttonToStored(f.button, buttonIndex) };
    case 'REPOST': return { type, channelId: f.channelId ?? null };
    case 'REACT': case 'REMOVE_REACTION': return { type, emoji: f.emoji ?? null };
    case 'GIVE_ROLES': case 'TAKE_ROLES': return { type, roleIds: [...asList(f.roleIds)] };
    case 'ADD_TO_THREAD': return { type, userIds: asList(f.users).map((u) => u.userId) };
    case 'SEND_EMBED': return { type, embedName: f.embedName ?? '', channelId: f.channelId ?? null };
    default:
      return REWARD_ACTIONS.includes(type) ? { type, amount: f.amount } : { type };
  }
}

const hasAutomationButton = (f) => MESSAGE_ACTIONS.includes(f?.type) && f.button?.kind === 'automation';

/**
 * The numbers of the automation buttons of a form action list, as /automation action add gives them: a button keeps the
 * number it has when that number is one of the stored automation's buttons; a new button (empty number) gets the next
 * number after both the row's `buttonSeq` and every number in use, so a removed button's number is never reused.
 * @param {object[]} actions form actions
 * @param {{ existing?: Iterable<number>, buttonSeq?: number, assign?: boolean }} stored the stored row's button numbers
 *   and buttonSeq; assign false only checks the numbers that are given
 * @returns {{ indexes: (number|null)[], buttonSeq: number } | { error: { path: string, message: string } }}
 *   indexes[i]: the number of action i's automation button (null: no automation button, or not assigned yet)
 */
export function planButtons(actions, { existing = [], buttonSeq = 0, assign = true } = {}) {
  const have = new Set(existing);
  const indexes = asList(actions).map(() => null);
  let high = Math.max(0, ...have);
  const used = new Set();
  for (const [i, a] of asList(actions).entries()) {
    if (!hasAutomationButton(a) || a.button.index == null) continue;
    if (!have.has(a.button.index)) {
      return { error: { path: `actions.${i}.button.index`, message: `Button number ${a.button.index} is not one of this automation's buttons. Leave it empty to get a new number.` } };
    }
    if (used.has(a.button.index)) {
      return { error: { path: `actions.${i}.button.index`, message: `Action ${i + 1}: button number ${a.button.index} is used twice.` } };
    }
    used.add(a.button.index);
    indexes[i] = a.button.index;
  }
  let seq = Number.isInteger(buttonSeq) ? buttonSeq : 0;
  for (const [i, a] of asList(actions).entries()) {
    if (!assign || !hasAutomationButton(a) || a.button.index != null) continue;
    const next = Math.max(seq, high) + 1;
    if (next > AUTOMATION_LIMITS.buttonIndex) return { error: { path: `actions.${i}.button`, message: BUTTONS_USED } };
    indexes[i] = next;
    seq = next;
    high = next;
  }
  return { indexes, buttonSeq: seq };
}

// ── Rules ────────────────────────────────────────────────────────────────────

const formatInt = (n) => n.toLocaleString('en-US');

/**
 * automationService.validateAutomationShape on stored shapes, with the field each problem is about. The `message`s are
 * the service's texts word for word (the drift test compares them).
 * @param {{ trigger: string, conditions: object[], actions: object[], cooldownSeconds: number }} shape
 * @returns {{ path: string, message: string }[]}
 */
export function shapeProblems({ trigger, conditions, actions, cooldownSeconds } = {}) {
  const out = [];
  if (!Object.hasOwn(TRIGGER_RULES, trigger ?? '')) return [{ path: 'trigger', message: 'Unknown trigger.' }];
  const conds = asList(conditions);
  const acts = asList(actions);
  const tLabel = TRIGGER_RULES[trigger].label;
  if (conds.length > LIMITS.AUTOMATION_CONDITIONS) out.push({ path: 'conditions', message: `An automation can have at most ${LIMITS.AUTOMATION_CONDITIONS} conditions.` });
  if (acts.length > LIMITS.AUTOMATION_ACTIONS) out.push({ path: 'actions', message: `An automation can have at most ${LIMITS.AUTOMATION_ACTIONS} actions.` });
  if (!Number.isInteger(cooldownSeconds) || cooldownSeconds < 0 || cooldownSeconds > AUTOMATION_LIMITS.cooldownSeconds) {
    out.push({ path: 'cooldownSeconds', message: `The cooldown must be a whole number of seconds from 0 to ${formatInt(AUTOMATION_LIMITS.cooldownSeconds)}.` });
  }

  const seenFlags = new Map();
  conds.forEach((c, k) => {
    const i = k + 1;
    const at = `conditions.${k}.type`;
    const def = Object.hasOwn(CONDITION_RULES, c?.type ?? '') ? CONDITION_RULES[c.type] : null;
    if (!def) { out.push({ path: at, message: `Condition ${i}: unknown type.` }); return; }
    if (!def.triggers.includes(trigger)) out.push({ path: at, message: `Condition ${i}: "${def.label}" doesn't work with the trigger "${tLabel}".` });
    if (def.noParams) {
      if (seenFlags.has(c.type)) out.push({ path: at, message: `Condition ${i} is already there.` });
      else seenFlags.set(c.type, k);
    }
  });
  if (seenFlags.has('IS_REPLY') && seenFlags.has('IS_NOT_REPLY')) {
    out.push({
      path: `conditions.${Math.max(seenFlags.get('IS_REPLY'), seenFlags.get('IS_NOT_REPLY'))}.type`,
      message: 'These two conditions can never both be true: "Message is a reply" and "Message is not a reply".',
    });
  }

  const usedButtons = new Set();
  let threadBefore = false;
  acts.forEach((a, k) => {
    const i = k + 1;
    const at = `actions.${k}`;
    const def = Object.hasOwn(ACTION_RULES, a?.type ?? '') ? ACTION_RULES[a.type] : null;
    if (!def) { out.push({ path: `${at}.type`, message: `Action ${i}: unknown type.` }); return; }
    if (!def.triggers.includes(trigger)) out.push({ path: `${at}.type`, message: `Action ${i}: "${def.label}" doesn't work with the trigger "${tLabel}".` });
    if ((a.type === 'SEND_MESSAGE' || a.type === 'SEND_EMBED') && !a.channelId && !S.CHANNEL.includes(trigger)) {
      out.push({ path: `${at}.channelId`, message: `Action ${i}: this trigger doesn't happen in a channel, so pick a channel.` });
    }
    if (a.type === 'CREATE_THREAD' && !a.channelId && !S.LIVE.includes(trigger)) {
      out.push({ path: `${at}.channelId`, message: `Action ${i}: there is no message to start a thread from, so pick a channel.` });
    }
    if (def.thread && !S.THREADY.includes(trigger) && !threadBefore) {
      out.push({ path: `${at}.type`, message: `Action ${i}: there is no thread here. Add "Create a thread" before it.` });
    }
    if (a.type === 'CREATE_THREAD') threadBefore = true;
    if (a.button && a.button.url == null && Number.isInteger(a.button.index)) {
      if (usedButtons.has(a.button.index)) out.push({ path: `${at}.button.index`, message: `Action ${i}: button number ${a.button.index} is used twice.` });
      usedButtons.add(a.button.index);
    }
  });
  return out;
}

/** The problem with one form condition's parameters (what /automation condition add asks for), or null. */
function conditionParamProblem(f) {
  switch (f.type) {
    case 'CHANNEL_IN': case 'CHANNEL_NOT_IN':
      return asList(f.channelIds).length ? null : { field: 'channelIds', message: 'Pick at least one channel.' };
    case 'CATEGORY_IN': case 'CATEGORY_NOT_IN':
      return asList(f.categoryIds).length ? null : { field: 'categoryIds', message: 'Pick at least one category.' };
    case 'CONTAINS': case 'NOT_CONTAINS':
      return asList(f.words).length ? null : { field: 'words', message: 'Add at least one word.' };
    case 'EXACT':
      return asList(f.texts).length ? null : { field: 'texts', message: 'Add at least one text.' };
    case 'MATCHES_REGEX': {
      if (typeof f.pattern !== 'string' || f.pattern === '') return { field: 'pattern', message: 'Write the regular expression.' };
      const v = validateRegex(f.pattern, flagsToStored(f.flags));
      return v.ok ? null : { field: 'pattern', message: `Invalid regex: ${v.reason}` };
    }
    case 'REACTED_WITH':
      return asList(f.emojis).length ? null : { field: 'emojis', message: 'Add at least one emoji.' };
    case 'USER_IS':
      return asList(f.users).length ? null : { field: 'users', message: 'Add at least one member.' };
    case 'BUTTON_IS':
      return asList(f.buttons).length ? null : { field: 'buttons', message: 'Add at least one button.' };
    default:
      return ROLE_CONDITIONS.includes(f.type) && !asList(f.roleIds).length ? { field: 'roleIds', message: 'Pick at least one role.' } : null;
  }
}

/** The problem with one form action's parameters (what /automation action add asks for), or null. */
function actionParamProblem(f) {
  if (MESSAGE_ACTIONS.includes(f.type)) {
    if (!nonEmpty(f.content)) return { field: 'content', message: 'Write the message.' };
    const b = f.button ?? {};
    if (b.kind === 'link' || b.kind === 'automation') {
      if (!nonEmpty(b.label)) return { field: 'button.label', message: 'Give the button a label.' };
      if (b.kind === 'link' && !nonEmpty(b.url)) return { field: 'button.url', message: 'Add the link the button opens.' };
    }
    return null;
  }
  switch (f.type) {
    case 'REPOST': return f.channelId ? null : { field: 'channelId', message: 'Pick the channel to repost to.' };
    case 'REACT': return f.emoji ? null : { field: 'emoji', message: 'Pick the emoji.' };
    case 'GIVE_ROLES': case 'TAKE_ROLES': return asList(f.roleIds).length ? null : { field: 'roleIds', message: 'Pick at least one role.' };
    case 'SEND_EMBED': return nonEmpty(f.embedName) ? null : { field: 'embedName', message: 'Type the name of the saved embed to send.' };
    default: return null;
  }
}

/** Form actions reduced to what shapeProblems reads (type, target channel, automation button). */
const shapeAction = (f) => ({
  type: f?.type,
  channelId: f?.type === 'CREATE_THREAD' ? f.threadChannelId : f?.channelId,
  button: hasAutomationButton(f) ? { url: null, index: f.button.index } : null,
});

/**
 * Cross-field rules of an automation (/automation and automationService.validateAutomationShape): conditions and actions
 * that fit the trigger, the parameters each type needs, button numbers, presets, and "on" needing an action. On update
 * `current` (the stored item) fills in what the request leaves out.
 * @returns {null|{ path: string, message: string }}
 */
export function checkAutomation(data, { mode, current } = {}) {
  if (mode !== 'update' && data.preset && data.preset !== 'none') return checkPreset(data);
  const t = mode === 'update' && current ? { ...current, ...data } : data;
  const conditions = asList(t.conditions);
  const actions = asList(t.actions);

  for (const [i, c] of conditions.entries()) {
    const p = conditionParamProblem(c);
    if (p) return { path: `conditions.${i}.${p.field}`, message: p.message };
  }
  for (const [i, a] of actions.entries()) {
    const p = actionParamProblem(a);
    if (p) return { path: `actions.${i}.${p.field}`, message: p.message };
  }

  // Button numbers: a number must be one of the stored automation's buttons (new buttons get theirs when saved).
  const existing = mode === 'update' && current
    ? asList(current.actions).filter(hasAutomationButton).map((a) => a.button.index).filter(Number.isInteger)
    : [];
  const plan = planButtons(actions, { existing, assign: false });
  if (plan.error) return plan.error;

  const shape = shapeProblems({
    trigger: t.trigger, conditions, actions: actions.map(shapeAction), cooldownSeconds: t.cooldownSeconds ?? 0,
  });
  if (shape.length) {
    const p = shape[0];
    // "Create a thread" keeps its channel in threadChannelId.
    const m = /^actions\.(\d+)\.channelId$/.exec(p.path);
    const path = m && actions[Number(m[1])]?.type === 'CREATE_THREAD' ? `actions.${m[1]}.threadChannelId` : p.path;
    return { path, message: p.message };
  }

  // On: at least one action (/automation enable). An update that only empties the actions turns it off instead, as
  // /automation action remove does with the last action.
  const turningOn = mode === 'update' ? data.enabled === true && current?.enabled !== true : t.enabled === true;
  const keepsOn = mode === 'update' && data.enabled === true && Object.hasOwn(data, 'actions');
  if ((turningOn || keepsOn) && actions.length === 0) return { path: 'enabled', message: NEEDS_ACTIONS };
  return null;
}

/** The /automation create preset options each preset needs. */
function checkPreset(data) {
  switch (data.preset) {
    case 'welcomer':
      return data.presetChannelId ? null : { path: 'presetChannelId', message: 'The Welcomer preset needs a channel to post in.' };
    case 'starboard':
      return data.presetChannelId ? null : { path: 'presetChannelId', message: 'The Starboard preset needs a channel to repost to.' };
    case 'thread-joiner':
      return data.presetUserId ? null : { path: 'presetUserId', message: 'The Thread Joiner preset needs a member to add to new threads.' };
    default:
      return { path: 'preset', message: 'Pick a preset from the list.' };
  }
}

// ── Items ────────────────────────────────────────────────────────────────────

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

/** The list line of an automation: its trigger and how many conditions and actions it has (as /automation list). */
export function summaryOf(row) {
  const trigger = TRIGGER_RULES[row?.trigger]?.label ?? String(row?.trigger ?? '');
  const conds = asList(row?.conditions).length;
  const acts = asList(row?.actions).length;
  return `${trigger} · ${conds ? plural(conds, 'condition') : 'always'} · ${plural(acts, 'action')}`;
}

/**
 * An Automation row (conditions and actions already normalised by automationService.normalizeList) as a collection item.
 * @param {object} row
 * @param {{ nameOf?: (automationId: string) => string|null, problems?: string[], creator?: string|null }} [extra]
 *   creator: the display name of row.createdBy (default: the id)
 */
export function automationItem(row, { nameOf = () => null, problems = [], creator = null } = {}) {
  return {
    id: row.id,
    name: row.name,
    preset: 'none',
    presetChannelId: null,
    presetMessage: null,
    presetRoleId: null,
    presetEmoji: null,
    presetThreshold: DEFAULT_THRESHOLD,
    presetUserId: null,
    trigger: row.trigger,
    conditions: asList(row.conditions).map((c) => conditionToForm(c, nameOf)),
    actions: asList(row.actions).map(actionToForm),
    cooldownSeconds: Number.isInteger(row.cooldownSeconds) ? row.cooldownSeconds : 0,
    enabled: row.enabled === true,
    summary: summaryOf(row),
    uses: Number.isInteger(row.uses) ? row.uses : 0,
    problems: [...problems],
    createdBy: creator ?? (typeof row.createdBy === 'string' && /^\d{17,20}$/.test(row.createdBy) ? row.createdBy : null),
    createdAt: iso(row.createdAt),
    updatedAt: iso(row.updatedAt),
  };
}
