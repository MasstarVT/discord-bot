// Descriptor of the `levelRoles` collection: the roles members earn at a level (/levelconfig role add|remove|list).
// Pure: constants and maths only. The rules mirror subRoleAdd in src/commands/admin/levelconfig.js.
import { LIMITS, XP_MAX, xpForLevel } from '../../config/constants.js';

/** /levelconfig role add accepts levels 1 to 999. */
export const LEVEL_INPUT_MAX = 999;

/** Total XP needed to reach `level` (the same sum as totalXpForLevel in xpService). */
export function totalXpForLevel(level) {
  let total = 0;
  for (let i = 0; i < level; i++) total += xpForLevel(i);
  return total;
}

/** The highest level whose total XP still fits in XP_MAX (= levelFromXp(XP_MAX)). */
export function highestReachableLevel() {
  let level = 0;
  let total = 0;
  while (total + xpForLevel(level) <= XP_MAX) {
    total += xpForLevel(level);
    level++;
  }
  return level;
}

const fields = Object.freeze([
  Object.freeze({
    field: 'level',
    type: 'int',
    label: 'Level',
    description: 'Members get the role as soon as they reach this level. Each level has one reward.',
    min: 1,
    max: LEVEL_INPUT_MAX,
    default: 1,
    required: true,
  }),
  Object.freeze({
    field: 'roleId',
    type: 'role',
    label: 'Reward role',
    description: "The role the bot gives. It must be below the bot's highest role, and each role can be the reward for one level only.",
    assignable: true,
    // /levelconfig role add allows a role with moderator permissions as a reward (allowDangerous).
    allowDangerous: true,
    default: null,
    required: true,
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({
    field: 'title', type: 'text', label: 'Reward', description: 'The level this reward is for.', maxLength: 40, default: '',
  }),
]);

export const collection = Object.freeze({
  key: 'levelRoles',
  module: 'leveling',
  label: 'Level rewards',
  singular: 'level reward',
  description: 'Roles members earn when they reach a level. Adding a reward does not give the role to members who are already '
    + 'past that level: run /levelconfig role sync in Discord for that. Members who already have a role keep it when you remove its reward. '
    + 'Whether lower rewards are kept is set under "Keep lower reward roles".',
  limit: LIMITS.LEVEL_ROLES,
  titleField: 'title',
  fields,
  createOnly: Object.freeze([]),
  readOnly,
  actions: Object.freeze([]),
  /** Levels nobody can reach (more XP than the XP cap allows) are refused, as /levelconfig role add does. */
  check(data, { current } = {}) {
    const level = data.level ?? current?.level;
    if (!Number.isInteger(level)) return null;
    if (totalXpForLevel(level) > XP_MAX) {
      return {
        path: 'level',
        message: `Level ${level} needs more than ${XP_MAX.toLocaleString('en-US')} XP, so nobody can reach it. `
          + `The highest reachable level is ${highestReachableLevel()}.`,
      };
    }
    return null;
  },
});
