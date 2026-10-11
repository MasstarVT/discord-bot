// Descriptor of the `starboards` collection: boards that repost popular messages (/starboard create|edit|delete|list).
// Pure: constants only; the limits that live in starboardService.js are copied here (the service opens Redis and Prisma at
// import) and test/dashboard-collections-more.test.js fails when they drift apart.
import { LIMITS } from '../../config/constants.js';
import { isSnowflake, validateValue } from '../schema.js';

export const MAX_EMOJIS_PER_BOARD = 10;
export const MAX_IGNORED_CHANNELS = 50;
export const BOARD_NAME_MAX = 32;

const EMOJI_FIELD = Object.freeze({ type: 'emoji', label: 'Emoji' });

/** Every channel kind a board can ignore (the command offers the same list). */
const SOURCE_KINDS = Object.freeze(['text', 'announcement', 'voice', 'stage', 'forum', 'media', 'category']);

const bool = (field, label, description, def) => Object.freeze({ field, type: 'boolean', label, description, default: def });

const fields = Object.freeze([
  Object.freeze({
    field: 'name',
    type: 'text',
    label: 'Name',
    description: 'Lowercase letters, digits, - and _ (up to 32). Unique on this server.',
    maxLength: BOARD_NAME_MAX,
    minLength: 1,
    pattern: '[a-z0-9_-]+',
    patternMessage: 'The name may only use lowercase letters a-z, digits, - and _.',
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'channelId',
    type: 'channel',
    label: 'Repost channel',
    description: 'Where popular messages are reposted. Changing it reposts live entries into the new channel; old reposts stay where they are.',
    channelTypes: Object.freeze(['text', 'announcement']),
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'emojis',
    type: 'stringList',
    label: 'Emojis that count',
    description: 'Reactions that count towards the threshold: an emoji, or a custom emoji as <:name:id> or just its ID. '
      + 'Empty means every emoji counts.',
    maxItems: MAX_EMOJIS_PER_BOARD,
    itemMaxLength: 64,
    default: Object.freeze(['⭐']),
  }),
  Object.freeze({
    field: 'threshold',
    type: 'int',
    label: 'Reactions needed',
    description: 'How many counting reactions a message needs to be reposted.',
    min: 1,
    max: 500,
    unit: 'reactions',
    default: 3,
  }),
  bool('selfStar', 'Count the author\'s own reaction', 'Off: a member reacting to their own message does not count.', false),
  bool('removeSelfStars', 'Remove the author\'s own reaction',
    'The bot removes a member\'s own qualifying reaction (needs Manage Messages). Cannot be combined with counting it.', false),
  bool('ignoreBots', 'Ignore bots', 'Ignore messages from bots and reactions by bots.', true),
  bool('removeBelowThreshold', 'Remove the repost below the threshold', 'Delete the repost when the message falls below the threshold again.', true),
  bool('autoReact', 'React to the repost',
    'The bot adds the first counting emoji to the repost (needs Add Reactions and Read Message History).', false),
  Object.freeze({
    field: 'maxMessageAgeDays',
    type: 'int',
    label: 'Ignore older messages',
    description: 'Messages older than this many days are never reposted. 0 means no limit.',
    min: 0,
    max: 3650,
    unit: 'days',
    default: 0,
  }),
  bool('allowNsfw', 'Repost from age-restricted channels', 'The repost channel must be age-restricted too.', false),
  Object.freeze({
    field: 'ignoredChannelIds',
    type: 'channels',
    label: 'Ignored channels',
    description: 'Messages in these channels (or categories) never reach this board.',
    channelTypes: SOURCE_KINDS,
    maxItems: MAX_IGNORED_CHANNELS,
    default: Object.freeze([]),
  }),
  bool('enabled', 'On', 'Off: nothing is reposted by this board.', true),
]);

const readOnly = Object.freeze([
  Object.freeze({
    field: 'reposts', type: 'int', label: 'Live reposts', description: 'Messages this board currently has reposted.',
    min: 0, max: 2_147_483_647, default: 0,
  }),
]);

/** A board's emoji entry: a unicode emoji, a custom emoji `<:name:id>` or the bare id of one. */
export function isEmojiEntry(entry) {
  if (isSnowflake(entry)) return true;
  try {
    validateValue(EMOJI_FIELD, entry, 'emojis');
    return true;
  } catch {
    return false;
  }
}

export const collection = Object.freeze({
  key: 'starboards',
  module: 'starboard',
  label: 'Starboards',
  singular: 'starboard',
  description: 'Boards that repost popular messages to a channel. Deleting a board keeps its reposts, but they stop updating. '
    + 'The bot picks up changes within about a minute.',
  limit: LIMITS.STARBOARDS,
  titleField: 'name',
  subtitleField: 'channelId',
  fields,
  createOnly: Object.freeze([]),
  readOnly,
  actions: Object.freeze([]),
  check(data, { mode, current } = {}) {
    const merged = { ...(mode === 'update' ? current : {}), ...data };
    if (data.emojis) {
      const bad = data.emojis.findIndex((e) => !isEmojiEntry(e));
      if (bad !== -1) return { path: `emojis.${bad}`, message: `"${String(data.emojis[bad]).slice(0, 40)}" is not an emoji.` };
    }
    if (merged.selfStar && merged.removeSelfStars) {
      return { path: 'removeSelfStars', message: 'Self-stars are counted on this board, so they cannot also be removed.' };
    }
    return null;
  },
});
