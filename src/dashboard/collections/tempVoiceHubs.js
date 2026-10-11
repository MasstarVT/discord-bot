// Descriptor of the `tempVoiceHubs` collection: join-to-create voice hubs (/voice-hub create|edit|delete|list|ignore-role|mod-role).
// Pure: constants only; the limits that live in tempVoiceService.js are copied here (the service opens Redis and Prisma at
// import) and test/dashboard-collections-more.test.js fails when they drift apart.
import { LIMITS } from '../../config/constants.js';

export const DEFAULT_NAME_FORMAT = "#{index} - {username}'s Channel";
export const MAX_HUB_ROLE_LIST = 10;

/** `{user}` renders as a mention, which a channel name cannot hold (validateNameFormat in tempVoiceService.js refuses it). */
const MENTION_TOKEN_RE = /\{\s*user(?:\.mention)?\s*\}|<[@#][!&]?\d+>/i;

const bool = (field, label, description, def) => Object.freeze({ field, type: 'boolean', label, description, default: def });

const fields = Object.freeze([
  Object.freeze({
    field: 'channelId',
    type: 'channel',
    label: 'Hub channel',
    description: 'The voice channel members join to get their own channel. Pick an existing voice channel; this page cannot create one '
      + '(/voice-hub create can do that in Discord). It cannot be changed later.',
    channelTypes: Object.freeze(['voice']),
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'categoryId',
    type: 'channel',
    label: 'Category for new channels',
    description: "Where the temporary channels are created. Empty uses the hub channel's own category.",
    channelTypes: Object.freeze(['category']),
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'nameFormat',
    type: 'text',
    label: 'Channel name',
    description: 'The name of each new channel. {index} is the channel number and {username} the owner\'s name. '
      + 'Mentions such as {user} are not allowed.',
    maxLength: 100,
    minLength: 1,
    default: DEFAULT_NAME_FORMAT,
    placeholders: Object.freeze(['{index}', '{username}']),
  }),
  Object.freeze({
    field: 'userLimit',
    type: 'int',
    label: 'User limit',
    description: 'How many members fit in a new channel. 0 means no limit.',
    min: 0,
    max: 99,
    unit: 'members',
    default: 0,
  }),
  Object.freeze({
    field: 'bitrate',
    type: 'int',
    label: 'Bitrate',
    description: "Audio quality of new channels. Empty uses Discord's default. The server's boost level caps it.",
    min: 8,
    max: 384,
    unit: 'kbps',
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'keepAliveMinutes',
    type: 'int',
    label: 'Keep empty channels',
    description: 'How long an empty channel survives before it is deleted. 0 deletes it at once.',
    min: 0,
    max: 10080,
    unit: 'minutes',
    default: 0,
  }),
  Object.freeze({
    field: 'ownershipLockMinutes',
    type: 'int',
    label: 'Ownership lock',
    description: 'How long after the owner leaves before someone else can claim the channel with /voice claim. 0 means at once.',
    min: 0,
    max: 1440,
    unit: 'minutes',
    default: 0,
  }),
  bool('syncPermissions', 'Copy the category\'s permissions',
    'On: new channels copy the category\'s permissions. Off: they copy the hub channel\'s.', true),
  bool('createTextChannel', 'Create a text channel too', 'Each new voice channel gets a companion text channel.', false),
  bool('textChannelMembersOnly', 'Text channel only for voice members',
    'Only members currently in the voice channel can see its text channel.', true),
  Object.freeze({
    field: 'ignoredRoleIds',
    type: 'roles',
    label: 'Protected roles',
    description: '/voice commands do not affect members with these roles: they cannot be kicked or banned, '
      + 'and locking or hiding does not shut them out.',
    maxItems: MAX_HUB_ROLE_LIST,
    default: Object.freeze([]),
  }),
  Object.freeze({
    field: 'moderatorRoleIds',
    type: 'roles',
    label: 'Moderator roles',
    description: 'Members with these roles can use every /voice command on any channel from this hub.',
    maxItems: MAX_HUB_ROLE_LIST,
    default: Object.freeze([]),
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({
    field: 'activeChannels', type: 'int', label: 'Open channels', description: 'Temporary channels from this hub that exist right now.',
    min: 0, max: 2_147_483_647, default: 0,
  }),
]);

export const collection = Object.freeze({
  key: 'tempVoiceHubs',
  module: 'tempvoice',
  label: 'Voice hubs',
  singular: 'voice hub',
  description: 'Join-to-create voice channels. Deleting a hub here only removes its settings and is refused while its temporary channels are open; '
    + 'the hub channel itself stays in Discord (use /voice-hub delete to remove channels too). '
    + 'The bot notices new, changed and deleted hubs within about 5 minutes.',
  limit: LIMITS.TEMPVOICE_HUBS,
  titleField: 'nameFormat',
  subtitleField: 'channelId',
  fields,
  createOnly: Object.freeze(['channelId']),
  readOnly,
  actions: Object.freeze([]),
  check(data) {
    if (data.nameFormat !== undefined && MENTION_TOKEN_RE.test(data.nameFormat)) {
      return {
        path: 'nameFormat',
        message: "Channel names can't contain mentions. Use {username} or {user.name} instead of {user}.",
      };
    }
    return null;
  },
});
