// Descriptor of the `balances` collection (Economy module): every member's coins on this server (EconomyAccount rows),
// richest first, with what /economy-admin coins give | remove | set and /economy-admin reset member: do. The writes are
// Discord-free, so the store calls economyService directly (single-statement SQL, the server's maximum balance, the
// one-time copy of the legacy global balance on a member's first touch). Pure.
import { COIN_MAX } from '../../config/constants.js';
import { avatarFor, fmt, profileName, toIso } from './levels.js';

const amountField = (label, description, min = 1) => Object.freeze({
  field: 'amount', type: 'int', label, description, min, max: COIN_MAX, default: 100, required: true,
});

/** /economy-admin reset `what` (coins: balance back to the starting balance, streak and cooldowns cleared; items: inventory). */
export const RESET_OPTIONS = Object.freeze([
  Object.freeze({ value: 'coins', label: 'Coins', description: 'Their balance goes back to the starting balance; their daily streak and cooldowns are cleared.' }),
  Object.freeze({ value: 'items', label: 'Items', description: 'Every item in their inventory is removed. Shop items stay.' }),
  Object.freeze({ value: 'all', label: 'Coins and items', description: 'Both of the above.' }),
]);

const fields = Object.freeze([
  Object.freeze({
    field: 'userId',
    type: 'user',
    label: 'Member',
    description: 'Who gets the coins. Bots cannot hold coins.',
    default: null,
    required: true,
  }),
  amountField('Coins to give', "Added to their balance, never above the server's maximum balance."),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'avatarUrl', type: 'url', label: 'Avatar', nullable: true, default: null }),
  Object.freeze({ field: 'name', type: 'text', label: 'Member', maxLength: 200, default: '' }),
  Object.freeze({ field: 'summary', type: 'text', label: 'Standing', maxLength: 200, default: '' }),
  Object.freeze({ field: 'rank', type: 'int', label: 'Rank', description: 'Members with the same balance share a rank.', min: 1, max: 2_147_483_647, nullable: true, default: null }),
  Object.freeze({ field: 'balance', type: 'int', label: 'Balance', min: 0, max: COIN_MAX, default: 0 }),
  Object.freeze({ field: 'dailyStreak', type: 'int', label: 'Daily streak', min: 0, max: 2_147_483_647, unit: 'days', default: 0 }),
  Object.freeze({ field: 'lastDaily', type: 'datetime', label: 'Last /daily', nullable: true, default: null }),
  Object.freeze({ field: 'lastWork', type: 'datetime', label: 'Last /work', nullable: true, default: null }),
  Object.freeze({ field: 'updatedAt', type: 'datetime', label: 'Last change', nullable: true, default: null }),
]);

export const collection = Object.freeze({
  key: 'balances',
  module: 'economy',
  label: 'Member balances',
  singular: 'member',
  description: "Every member's coins on this server, richest first. Give, remove or set coins and reset members like "
    + "/economy-admin does: balances never go above the server's maximum balance, and a member's first balance here starts "
    + 'from their old global balance unless that import is turned off. This works while the Economy module is off.',
  limit: null,
  searchable: true,
  searchPlaceholder: 'Search by member name or user ID',
  titleField: 'name',
  subtitleField: 'summary',
  avatarField: 'avatarUrl',
  canEdit: false,
  canDelete: false,
  fields,
  createOnly: Object.freeze(['userId', 'amount']),
  readOnly,
  actions: Object.freeze([
    Object.freeze({
      id: 'give',
      label: 'Give coins',
      description: "Adds coins to this member's balance (like /economy-admin coins give).",
      fields: Object.freeze([amountField('Coins to give', "Never above the server's maximum balance.")]),
    }),
    Object.freeze({
      id: 'take',
      label: 'Remove coins',
      description: 'Takes coins from this member, never below 0 (like /economy-admin coins remove).',
      fields: Object.freeze([amountField('Coins to remove', 'If they have fewer, their balance becomes 0.')]),
    }),
    Object.freeze({
      id: 'set',
      label: 'Set balance',
      description: "Sets this member's balance (like /economy-admin coins set).",
      fields: Object.freeze([Object.freeze({
        field: 'balance', type: 'int', label: 'New balance', description: "From 0 to the server's maximum balance.",
        min: 0, max: COIN_MAX, default: 0, required: true,
      })]),
    }),
    Object.freeze({
      id: 'reset',
      label: 'Reset',
      description: "Resets this member's coins and/or items (like /economy-admin reset member:).",
      danger: true,
      confirm: 'This cannot be undone.',
      fields: Object.freeze([Object.freeze({
        field: 'what', type: 'enum', label: 'What to reset', options: RESET_OPTIONS, default: 'coins', required: true,
      })]),
    }),
  ]),
});

/**
 * An EconomyAccount row as a list item.
 * @param {{ userId: string, balance: number, dailyStreak?: number, lastDaily?: Date|null, lastWork?: Date|null, updatedAt?: Date|null }} row
 * @param {{ rank?: number|null, profile?: object|null, currency?: { name: string, emoji: string } }} [extra]
 */
export function balanceItem(row, { rank = null, profile = null, currency = { name: 'coins', emoji: '🪙' } } = {}) {
  return {
    id: row.userId,
    userId: row.userId,
    amount: null,
    avatarUrl: avatarFor(profile, row.userId),
    name: profileName(profile, row.userId),
    summary: [`${currency.emoji} ${fmt(row.balance)} ${currency.name}`, rank ? `#${fmt(rank)}` : null].filter(Boolean).join(' · '),
    rank,
    balance: row.balance,
    dailyStreak: row.dailyStreak ?? 0,
    lastDaily: toIso(row.lastDaily),
    lastWork: toIso(row.lastWork),
    updatedAt: toIso(row.updatedAt),
  };
}
