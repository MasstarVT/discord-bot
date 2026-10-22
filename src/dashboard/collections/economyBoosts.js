// Descriptor of the `economyBoosts` collection: coin boosts for /daily and /work (/economy-admin boost add|remove|toggle).
// Pure: constants only; the limits that live in economyService.js are copied here and test/dashboard-collections-more.test.js
// fails when they drift apart.
import { LIMITS } from '../../config/constants.js';

export const BOOST_NAME_MAX = 32;
export const BOOST_PERCENT_MAX = 500;

const fields = Object.freeze([
  Object.freeze({
    field: 'name',
    type: 'text',
    label: 'Name',
    description: 'Unique on this server (upper and lower case count as the same).',
    maxLength: BOOST_NAME_MAX,
    minLength: 1,
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'percent',
    type: 'int',
    label: 'Bonus',
    description: 'Extra coins in percent: 50 turns a payout of 100 coins into 150.',
    min: 1,
    max: BOOST_PERCENT_MAX,
    unit: '%',
    default: 10,
    required: true,
  }),
  Object.freeze({
    field: 'period',
    type: 'enum',
    label: 'How often one member can use it',
    description: 'One-time: once per member, ever. The others: once per day, ISO week or month in the server time zone. '
      + 'Cannot be changed later; delete the boost and create a new one for that.',
    options: Object.freeze([
      Object.freeze({ value: 'ONE_TIME', label: 'One-time' }),
      Object.freeze({ value: 'DAILY', label: 'Daily' }),
      Object.freeze({ value: 'WEEKLY', label: 'Weekly' }),
      Object.freeze({ value: 'MONTHLY', label: 'Monthly' }),
    ]),
    default: 'DAILY',
  }),
  Object.freeze({
    field: 'roleId',
    type: 'role',
    label: 'Only members with this role',
    description: 'Leave empty so every member qualifies. The role is only checked, never given.',
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'enabled',
    type: 'boolean',
    label: 'On',
    description: 'A boost that is off is ignored. Its usage history is kept.',
    default: true,
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'summary', type: 'text', label: 'Summary', maxLength: 100, default: '' }),
  Object.freeze({
    field: 'uses', type: 'int', label: 'Times used', description: 'How many times members have used this boost so far.',
    min: 0, max: 2_147_483_647, default: 0,
  }),
]);

export const collection = Object.freeze({
  key: 'economyBoosts',
  module: 'economy',
  label: 'Coin boosts',
  singular: 'coin boost',
  description: 'A boost multiplies one /daily or /work payout for a member who holds its role (or for everyone). '
    + 'Only the highest boost a member has not used up in the current period applies. Rewards, /pay, casino winnings and '
    + 'admin grants are never boosted.',
  limit: LIMITS.ECONOMY_BOOSTS,
  titleField: 'name',
  subtitleField: 'summary',
  fields,
  createOnly: Object.freeze(['period']),
  readOnly,
  actions: Object.freeze([]),
});
