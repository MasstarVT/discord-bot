// Descriptor of the `achievements` collection (/achievement-config create|edit|delete|enable|disable|tier ...).
// Pure: constants only. The limits that live in achievementService.js are copied here (the service opens Redis and Prisma at
// import) and test/dashboard-collections-more.test.js fails when they drift apart.
// Each tier is one flat object: its goal plus the four reward kinds the command offers (role to give, role to take,
// XP, coins). The tier's name is its position: Bronze, Silver, Gold, Diamond.
import { LIMITS } from '../../config/constants.js';

export const NAME_MAX = 50;
export const DESCRIPTION_MAX = 200;
export const GOAL_INPUT_MAX = 10_000_000;
export const MAX_REWARD_AMOUNT = 1_000_000;
export const DEADLINE_MIN_MS = 60_000;
export const DEADLINE_MAX_MS = 5 * 365 * 86_400_000;
/** Highest goal per trigger (GOAL_MAX in achievementService.js). */
export const GOAL_MAX = Object.freeze({ MESSAGES: 10_000_000, REACTIONS: 10_000_000, LEVEL: 1000, BOOST: 100, PURCHASES: 1_000_000 });
export const TRIGGER_LABELS = Object.freeze({
  MESSAGES: 'Messages sent', REACTIONS: 'Reactions added', LEVEL: 'Level reached', BOOST: 'Server boosts', PURCHASES: 'Shop items bought',
});
export const TIER_NAMES = Object.freeze(['Bronze', 'Silver', 'Gold', 'Diamond']);

const tierFields = Object.freeze([
  Object.freeze({
    field: 'goal',
    type: 'int',
    label: 'Goal',
    description: 'What a member has to reach for this tier. Goals go up from tier to tier.',
    min: 1,
    max: GOAL_INPUT_MAX,
    default: 100,
    required: true,
  }),
  Object.freeze({
    field: 'roleAdd',
    type: 'role',
    label: 'Give this role',
    description: "Reward: a role the bot gives. It must be below the bot's highest role.",
    assignable: true,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'roleRemove',
    type: 'role',
    label: 'Take this role away',
    description: "Reward: a role the bot removes. It must be below the bot's highest role.",
    assignable: true,
    // /achievement-config allows any role below the bot to be taken away (allowDangerous).
    allowDangerous: true,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'xp',
    type: 'int',
    label: 'XP reward',
    description: 'Reward: XP. Needs the Levels module to be on.',
    min: 1,
    max: MAX_REWARD_AMOUNT,
    unit: 'XP',
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'coins',
    type: 'int',
    label: 'Coin reward',
    description: 'Reward: coins. Needs the Economy module to be on.',
    min: 1,
    max: MAX_REWARD_AMOUNT,
    unit: 'coins',
    nullable: true,
    default: null,
  }),
]);

const fields = Object.freeze([
  Object.freeze({
    field: 'name',
    type: 'text',
    label: 'Name',
    description: 'Unique on this server (upper and lower case count as the same).',
    maxLength: NAME_MAX,
    minLength: 1,
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'description',
    type: 'text',
    label: 'Description',
    description: 'Short text shown in /achievements.',
    maxLength: DESCRIPTION_MAX,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'emoji',
    type: 'emoji',
    label: 'Badge',
    description: 'One emoji shown next to the name. Empty uses the default badge of the trigger. '
      + 'A custom emoji must come from a server the bot is in.',
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'trigger',
    type: 'enum',
    label: 'What members do',
    description: 'What counts as progress. Cannot be changed later. '
      + 'Level achievements need the Levels module, purchase achievements the Economy module.',
    options: Object.freeze(Object.entries(TRIGGER_LABELS).map(([value, label]) => Object.freeze({ value, label }))),
    default: 'MESSAGES',
    required: true,
  }),
  Object.freeze({
    field: 'tiers',
    type: 'objectList',
    label: 'Tiers',
    description: 'Up to four goals, lowest first: Bronze, Silver, Gold, Diamond. Members unlock a tier when they reach its goal and get its rewards. '
      + 'Tiers are matched by position when you change them: removing the last tier keeps rewards already given, '
      + 'and members who already passed a lowered goal unlock it with their next activity.',
    itemFields: tierFields,
    minItems: 1,
    maxItems: LIMITS.ACHIEVEMENT_TIERS,
    default: Object.freeze([Object.freeze({ goal: 100, roleAdd: null, roleRemove: null, xp: null, coins: null })]),
    required: true,
  }),
  Object.freeze({
    field: 'trackPastProgress',
    type: 'boolean',
    label: 'Count past activity',
    description: 'Off: members start from zero when the achievement is created. Only applies to members who have no progress yet.',
    default: true,
  }),
  Object.freeze({
    field: 'deadline',
    type: 'datetime',
    label: 'Deadline',
    description: 'The last moment members can make progress (between a minute and five years from now). Empty means no deadline.',
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'enabled',
    type: 'boolean',
    label: 'On',
    description: 'Off: nothing is counted until you turn it back on. Progress is kept.',
    default: true,
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'triggerLabel', type: 'text', label: 'Trigger', maxLength: 40, default: '' }),
  Object.freeze({
    field: 'members', type: 'int', label: 'Members with progress', description: 'How many members have started this achievement.',
    min: 0, max: 2_147_483_647, default: 0,
  }),
]);

/** @returns {{ path: string, message: string }|null} */
function checkTiers(tiers, trigger) {
  const max = GOAL_MAX[trigger];
  let previous = 0;
  for (const [i, tier] of tiers.entries()) {
    const label = TIER_NAMES[i] ?? `Tier ${i + 1}`;
    if (max !== undefined && tier.goal > max) {
      return { path: `tiers.${i}.goal`, message: `${label}: ${TRIGGER_LABELS[trigger]} goals can be at most ${max.toLocaleString('en-US')}.` };
    }
    if (tier.goal <= previous) {
      return {
        path: `tiers.${i}.goal`,
        message: `${label}: goals must go up from tier to tier (this one has to be above ${previous.toLocaleString('en-US')}).`,
      };
    }
    previous = tier.goal;
    if (tier.roleAdd && tier.roleAdd === tier.roleRemove) {
      return { path: `tiers.${i}.roleRemove`, message: `${label}: a tier cannot give and take the same role.` };
    }
  }
  return null;
}

/** @returns {{ path: string, message: string }|null} */
function checkDeadline(deadline, now = Date.now()) {
  const delta = Date.parse(deadline) - now;
  if (delta < DEADLINE_MIN_MS) return { path: 'deadline', message: 'The deadline must be in the future.' };
  if (delta > DEADLINE_MAX_MS) return { path: 'deadline', message: 'The deadline can be at most 5 years away.' };
  return null;
}

export const collection = Object.freeze({
  key: 'achievements',
  module: 'achievements',
  label: 'Achievements',
  singular: 'achievement',
  description: 'Goals members work towards (messages, reactions, levels, boosts, purchases), with up to four tiers and rewards. '
    + 'Deleting an achievement deletes every member\'s progress on it; roles, XP and coins already given are kept. '
    + 'The bot picks up changes within about 30 seconds.',
  limit: LIMITS.ACHIEVEMENTS,
  titleField: 'name',
  subtitleField: 'triggerLabel',
  fields,
  createOnly: Object.freeze(['trigger']),
  readOnly,
  actions: Object.freeze([]),
  check(data, { mode, current } = {}) {
    const trigger = data.trigger ?? current?.trigger;
    if (data.tiers) {
      const problem = checkTiers(data.tiers, trigger);
      if (problem) return problem;
    }
    if (data.deadline) {
      // An unchanged deadline that has passed stays editable (the achievement simply shows as ended).
      const unchanged = mode === 'update' && current?.deadline && Date.parse(current.deadline) === Date.parse(data.deadline);
      if (!unchanged) {
        const problem = checkDeadline(data.deadline);
        if (problem) return problem;
      }
    }
    return null;
  },
});
