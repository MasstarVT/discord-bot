import prisma from '../database/client.js';

export const STARTING_BALANCE  = 1_000;
export const DAILY_AMOUNT      = 200;
export const DAILY_COOLDOWN_MS = 24 * 60 * 60 * 1000;
export const PITY_THRESHOLD    = 100;
export const PITY_AMOUNT       = 500;
export const PITY_COOLDOWN_MS  = 12 * 60 * 60 * 1000;

export async function getOrCreate(userId) {
  return prisma.userEconomy.upsert({
    where:  { userId },
    create: { userId, balance: STARTING_BALANCE },
    update: {},
  });
}

export async function getBalance(userId) {
  const row = await getOrCreate(userId);
  return row.balance;
}

// Apply a finished bet: deducts the bet then adds back the payout.
// Returns the updated balance.
export async function applyBet(userId, bet, payout) {
  const net = payout - bet;
  const row = await prisma.userEconomy.update({
    where: { userId },
    data:  { balance: { increment: net } },
  });
  return row.balance;
}

// Returns { claimed: bool, balance?: number, nextAt?: Date, tooRich?: bool }
export async function claimPity(userId) {
  const row = await getOrCreate(userId);

  if (row.balance >= PITY_THRESHOLD)
    return { claimed: false, tooRich: true, balance: row.balance };

  const now    = Date.now();
  const nextAt = row.lastPity ? row.lastPity.getTime() + PITY_COOLDOWN_MS : 0;
  if (now < nextAt) return { claimed: false, nextAt: new Date(nextAt) };

  const updated = await prisma.userEconomy.update({
    where: { userId },
    data:  { balance: { increment: PITY_AMOUNT }, lastPity: new Date() },
  });
  return { claimed: true, balance: updated.balance };
}

// Returns { claimed: bool, balance?: number, nextAt?: Date }
export async function claimDaily(userId) {
  const row   = await getOrCreate(userId);
  const now   = Date.now();
  const nextAt = row.lastDaily ? row.lastDaily.getTime() + DAILY_COOLDOWN_MS : 0;

  if (now < nextAt) return { claimed: false, nextAt: new Date(nextAt) };

  const updated = await prisma.userEconomy.update({
    where: { userId },
    data:  { balance: { increment: DAILY_AMOUNT }, lastDaily: new Date() },
  });
  return { claimed: true, balance: updated.balance };
}
