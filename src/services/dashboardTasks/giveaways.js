// The `dashboard.task` kinds of the Giveaways module: what the web dashboard asks the bot to do with /giveaway's own
// service functions (giveawayService.js), so the message with the Participate button, the end job, the draw, the
// announcement, the prizes and the cap all behave exactly as when a moderator runs the command.
//   giveaway.start   /giveaway start    payload: the form's values (prize, channelId, winnerCount, duration | endsAt, ...)
//   giveaway.end     /giveaway end      payload: { giveawayId }
//   giveaway.reroll  /giveaway reroll   payload: { giveawayId, winners }
//   giveaway.cancel  /giveaway cancel   payload: { giveawayId }
//   giveaway.restart /giveaway restart  payload: { giveawayId, channelId?, duration? | endsAt? }
//   giveaway.odds    /giveaway odds     payload: { giveawayId, roleWeights } (the complete new list)
// Every task re-checks what the router and the command check: the module is on, the dashboard user is still in the server and
// is at least a moderator, then the command's own rules for each option. The factory takes test doubles (`deps`).
import { ChannelType, PermissionFlagsBits } from 'discord.js';
import prisma from '../../database/client.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES } from '../../config/modules.js';
import { checkChannelUsable, permissionNames, pingableRoleIds } from '../../utils/interactionHelpers.js';
import { parseDuration } from '../../utils/parseDuration.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { clampText } from '../../utils/text.js';
import * as giveawayService from '../giveawayService.js';
import { getSettingsSafe, isModuleEnabled } from '../settingsService.js';
import { TaskError } from './shared.js';

const TEXT_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const SNOWFLAKE_RE = /^\d{17,20}$/;
const GIVEAWAY_ID_RE = /^c[a-z0-9]{20,31}$/;
const PRIZE_MAX = 256;
const MAX_GATE_ROLES = 10;
const GENERIC_ERROR = 'Discord or the database did not respond. Please try again in a moment.';
const INVALID = 'Those giveaway details were not valid. Reload the page and try again.';

const isId = (v) => typeof v === 'string' && SNOWFLAKE_RE.test(v);
const invalid = () => new TaskError(INVALID);

function idList(value, max) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value) || value.length > max || !value.every(isId)) throw invalid();
  return [...new Set(value)];
}

function optionalInt(value, min, max) {
  if (value === undefined || value === null) return null;
  if (!Number.isInteger(value) || value < min || value > max) throw invalid();
  return value;
}

function optionalText(value, max) {
  if (value === undefined || value === null) return null;
  if (typeof value !== 'string' || value.length > max) throw invalid();
  return value.trim() || null;
}

function giveawayId(payload) {
  const id = payload?.giveawayId;
  if (typeof id !== 'string' || !GIVEAWAY_ID_RE.test(id)) throw new TaskError('That giveaway no longer exists.');
  return id;
}

/** Names of up to five members for a toast: "alice, bob and 3 more". */
function nameList(guild, ids) {
  const names = ids.slice(0, 5).map((id) => guild.members?.cache?.get(id)?.displayName ?? `user ${id}`);
  const more = ids.length - names.length;
  return more > 0 ? `${names.join(', ')} and ${more} more` : names.join(', ');
}

/** A bot error text that the Discord commands format with markdown, as plain text for a toast. */
const plain = (text, channel) => String(text ?? '').replace(/\*\*/g, '').replace(/<#\d+>/g, channel ? `#${channel.name}` : 'the channel');

const utc = (d) => `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC`;

/**
 * Builds the giveaway tasks.
 * @param {{ service?: object, getSettings?: Function, db?: object, now?: () => Date }} [deps] test doubles
 */
export function createGiveawayTasks(deps = {}) {
  const svc = { ...giveawayService, ...deps.service };
  const getSettings = deps.getSettings ?? getSettingsSafe;
  const db = deps.db ?? prisma;
  const now = deps.now ?? (() => new Date());

  /** One entry of the bonus odds: a role id and 2..10 entries. */
  const validWeight = (w) => Boolean(w) && isId(w.roleId) && Number.isInteger(w.weight)
    && w.weight >= svc.MIN_BONUS_WEIGHT && w.weight <= svc.MAX_WEIGHT;

  /** The router's gate and the command's level: the module is on, the user is in the server and is a moderator. */
  async function access({ guild, actor }) {
    if (!actor?.member && !actor?.isBotOwner) throw new TaskError('You are no longer a member of this server.');
    const settings = await getSettings(guild.id);
    if (!settings) throw new TaskError('The server settings could not be loaded. Try again in a moment.');
    if (!isModuleEnabled(settings, 'giveaways')) {
      throw new TaskError(`The ${MODULES.giveaways.emoji} ${MODULES.giveaways.label} module is turned off on this server. Turn it on first.`);
    }
    const level = actor.isBotOwner ? PERMISSION_LEVELS.OWNER : resolvePermissionLevel(actor.member, settings);
    if (level < PERMISSION_LEVELS.MODERATOR) throw new TaskError('You need to be a moderator of this server to manage giveaways.');
    return settings;
  }

  async function loadGiveaway(guild, id) {
    const g = await db.giveaway.findFirst({ where: { id, guildId: guild.id } });
    if (!g) throw new TaskError('That giveaway no longer exists.');
    return g;
  }

  async function roleOf(guild, id) {
    const cached = guild.roles?.cache?.get(id);
    if (cached) return cached;
    if (typeof guild.roles?.fetch !== 'function') return null;
    return (await guild.roles.fetch(id).catch(() => null)) ?? null;
  }

  /** The channel the giveaway is posted in, with the command's checks for the bot and for the dashboard user. */
  async function postChannel(guild, actor, channelId, { missing = 'That channel no longer exists. Pick another one.' } = {}) {
    let channel;
    try {
      channel = await svc.fetchChannel(guild, channelId);
    } catch (err) {
      // 50001 / 50013: the channel exists but the bot may not see it. Anything else is a real error (logged by the runner).
      if (err?.code === 50001 || err?.code === 50013) {
        throw new TaskError("I can't see that channel. Give me View Channel there, or pick another one.");
      }
      throw err;
    }
    if (!channel) throw new TaskError(missing);
    if (!TEXT_TYPES.includes(channel.type)) throw new TaskError('Giveaways can be posted in text or announcement channels only.');
    if (actor.member && !channel.permissionsFor(actor.member)?.has([PermissionFlagsBits.ViewChannel, PermissionFlagsBits.SendMessages])) {
      throw new TaskError(`You can't send messages in #${channel.name}.`);
    }
    const usable = checkChannelUsable(channel, { needs: [...svc.POST_NEEDS] });
    if (!usable.ok) {
      throw new TaskError(usable.missing.length
        ? `I need ${permissionNames(usable.missing).join(', ')} in #${channel.name} to run a giveaway there.`
        : `I can't use #${channel.name}: ${plain(usable.reason, channel).replace(/\.$/, '')}.`);
    }
    return channel;
  }

  /** `duration` (a length from now) or `endsAt` (an exact moment) as a Date, checked like /giveaway start's `duration`. */
  function endTime({ duration, endsAt }, settings, at) {
    if (duration && endsAt) throw new TaskError('Use either the length or the end time, not both.');
    let date;
    if (duration) {
      const ms = parseDuration(duration);
      if (ms === null) throw new TaskError('Use a length like 30m, 2h, 3d or 1w.');
      date = new Date(at.getTime() + ms);
    } else if (endsAt) {
      date = new Date(Date.parse(endsAt));
    } else {
      throw new TaskError('Set how long the giveaway runs, or when it ends.');
    }
    const v = svc.validateEndsAt(date, at, settings.timezone);
    if (!v.ok) throw new TaskError(v.message);
    return date;
  }

  /** A role that must exist and must not be @everyone. */
  async function pickRole(guild, id, what) {
    if (id === guild.id) throw new TaskError(`Choose a specific role for ${what}, not @everyone.`);
    const role = await roleOf(guild, id);
    if (!role) throw new TaskError(`A role chosen for ${what} no longer exists. Pick another one.`);
    return role;
  }

  function failStartOrRestart(res, channel) {
    if (res.code === 'LIMIT') {
      return new TaskError(`This server already has ${LIMITS.ACTIVE_GIVEAWAYS} running or draft giveaways. End or cancel one first.`);
    }
    if (res.code === 'SEND_FAILED') return new TaskError(`I couldn't post the giveaway in #${channel.name}. Check my permissions there.`);
    return new TaskError(GENERIC_ERROR);
  }

  // ── giveaway.start ──────────────────────────────────────────────────────────

  async function start(ctx) {
    const { guild, client, actor, payload } = ctx;
    const settings = await access(ctx);

    // The payload is the dashboard's validated form; it is read again as untrusted JSON.
    if (typeof payload?.prize !== 'string') throw invalid();
    const prize = payload.prize.trim();
    if (!prize) throw new TaskError('The prize cannot be empty.');
    if (prize.length > PRIZE_MAX) throw new TaskError(`The prize can be at most ${PRIZE_MAX} characters.`);
    const description = optionalText(payload.description, 1000);
    if (!isId(payload.channelId)) throw invalid();
    const winnerCount = payload.winnerCount === undefined ? 1 : payload.winnerCount;
    if (!Number.isInteger(winnerCount) || winnerCount < 1 || winnerCount > LIMITS.GIVEAWAY_WINNERS) {
      throw new TaskError(`Pick between 1 and ${LIMITS.GIVEAWAY_WINNERS} winners.`);
    }
    const duration = optionalText(payload.duration, 40);
    if (payload.endsAt !== undefined && payload.endsAt !== null && (typeof payload.endsAt !== 'string' || payload.endsAt.length > 40)) {
      throw invalid();
    }
    const requiredIds = idList(payload.requiredRoleIds, MAX_GATE_ROLES);
    const deniedIds = idList(payload.deniedRoleIds, MAX_GATE_ROLES);
    const rawWeights = payload.roleWeights ?? [];
    if (!Array.isArray(rawWeights) || rawWeights.length > svc.MAX_ROLE_WEIGHTS) throw invalid();
    const weightIds = rawWeights.map((w) => {
      if (!validWeight(w)) throw invalid();
      return w.roleId;
    });
    if (new Set(weightIds).size !== weightIds.length) throw new TaskError('Each role can have bonus odds only once.');
    const xpPrize = optionalInt(payload.xpPrize, 1, svc.MAX_PRIZE_AMOUNT);
    const coinPrize = optionalInt(payload.coinPrize, 1, svc.MAX_PRIZE_AMOUNT);
    const pingRoleId = payload.pingRoleId ?? null;
    if (pingRoleId !== null && !isId(pingRoleId)) throw invalid();
    const embedColor = optionalInt(payload.embedColor, 0, 0xffffff);

    // 1-3. Channel, dashboard user's access, bot permissions.
    const channel = await postChannel(guild, actor, payload.channelId);

    // 4. Duration.
    const endsAt = endTime({ duration, endsAt: payload.endsAt ?? null }, settings, now());

    // 6. Required and blocked roles (managed roles are fine: nothing is assigned).
    for (const id of requiredIds) await pickRole(guild, id, 'the required roles');
    for (const id of deniedIds) await pickRole(guild, id, 'the blocked roles');
    if (requiredIds.some((id) => deniedIds.includes(id))) throw new TaskError('The same role cannot be both required and blocked.');

    // 7. Bonus odds.
    for (const id of weightIds) await pickRole(guild, id, 'the bonus odds');
    const roleWeights = rawWeights.map((w) => ({ roleId: w.roleId, weight: w.weight }));

    // 8. XP and coin prizes.
    for (const [kind, amount] of [['xp', xpPrize], ['coins', coinPrize]]) {
      if (amount === null) continue;
      const refusal = svc.rewardUnavailableMessage(kind, settings);
      if (refusal) throw new TaskError(plain(refusal));
    }

    // 9. Ping role: the bot sends the ping, and the dashboard user must be allowed to ping it too.
    if (pingRoleId) {
      const role = await pickRole(guild, pingRoleId, 'the role to ping');
      if (pingableRoleIds(channel, guild.members?.me ?? null, [pingRoleId]).refused.length) {
        throw new TaskError(`I can't ping @${role.name} in #${channel.name}. Make the role mentionable, `
          + 'or give me Mention @everyone, @here and All Roles there.');
      }
      if (actor.member && pingableRoleIds(channel, actor.member, [pingRoleId]).refused.length) {
        throw new TaskError(`You don't have permission to ping @${role.name} in #${channel.name}.`);
      }
    }

    // 12-13. Start (posts the message and schedules `giveaway.end`).
    const res = await svc.startGiveaway({
      client, guild, channel, hostId: actor.id, prize, description, winnerCount, endsAt, requiredRoleIds: requiredIds,
      deniedRoleIds: deniedIds, roleWeights, xpPrize, coinPrize, pingRoleId, embedColor, settings,
    });
    if (!res.ok) throw failStartOrRestart(res, channel);
    return {
      message: `Started the giveaway for "${clampText(prize, 80)}" in #${channel.name}. It ends ${utc(endsAt)}.`,
      data: { giveawayId: res.giveaway.id },
    };
  }

  // ── giveaway.end ────────────────────────────────────────────────────────────

  async function end(ctx) {
    const { guild, client, payload } = ctx;
    await access(ctx);
    const g = await loadGiveaway(guild, giveawayId(payload));
    if (g.status !== 'ACTIVE') {
      throw new TaskError(g.status === 'DRAFT'
        ? 'That giveaway is a draft (its message was deleted). Restart it first.'
        : `That giveaway is ${svc.STATUS_TEXT[g.status]}.`);
    }
    const res = await svc.endGiveaway({ client, guild, giveawayId: g.id, source: 'command' });
    const prize = clampText(g.prize, 80);
    if (res.ok) {
      let message = res.winnerIds.length
        ? `Ended "${prize}". ${res.winnerIds.length === 1 ? 'Winner' : 'Winners'}: ${nameList(guild, res.winnerIds)}.`
        : `Ended "${prize}" with no valid entrants.`;
      if (res.payWarnings?.length) message += ` ${plain(res.payWarnings[0]).replace(/^⚠️\s*/, 'Note: ')}`;
      if (res.announced === false) message += ' I could not post the announcement in the giveaway channel.';
      return { message, data: { giveawayId: g.id } };
    }
    switch (res.code) {
      case 'NOT_ACTIVE':
      case 'NOT_FOUND':
        throw new TaskError('That giveaway was already ended.');
      case 'DRAFTED':
        throw new TaskError('The giveaway message or its channel was deleted, so the giveaway was moved to drafts. '
          + 'Restart it to post it again.');
      case 'NO_ACCESS':
        throw new TaskError(res.missing?.length
          ? `I need ${permissionNames(res.missing).join(', ')} in the giveaway channel to end this giveaway.`
          : plain(res.reason ?? "I can't use the giveaway channel to end this giveaway."));
      case 'RESOLVE_FAILED':
        throw new TaskError("I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.");
      default:
        throw new TaskError(GENERIC_ERROR);
    }
  }

  // ── giveaway.reroll ─────────────────────────────────────────────────────────

  async function reroll(ctx) {
    const { guild, payload } = ctx;
    await access(ctx);
    const g = await loadGiveaway(guild, giveawayId(payload));
    if (g.status !== 'ENDED') {
      throw new TaskError(g.status === 'ACTIVE'
        ? 'That giveaway is still running. End it first.'
        : 'Only ended giveaways can be rerolled.');
    }
    const count = payload.winners === undefined ? 1 : payload.winners;
    if (!Number.isInteger(count) || count < 1 || count > LIMITS.GIVEAWAY_WINNERS) {
      throw new TaskError(`Pick between 1 and ${LIMITS.GIVEAWAY_WINNERS} winners.`);
    }
    const res = await svc.rerollGiveaway({ client: ctx.client, guild, giveawayId: g.id, count });
    if (res.ok) {
      const n = res.newWinnerIds.length;
      let message = `New winner${n === 1 ? '' : 's'} of "${clampText(g.prize, 80)}": ${nameList(guild, res.newWinnerIds)}.`;
      if (res.payWarnings?.length) message += ` ${plain(res.payWarnings[0]).replace(/^⚠️\s*/, 'Note: ')}`;
      if (res.announced === false) message += ' I could not post the announcement in the giveaway channel.';
      return { message, data: { giveawayId: g.id } };
    }
    switch (res.code) {
      case 'NO_ENTRANTS':
        throw new TaskError('There are no other eligible entrants to draw from.');
      case 'NOT_ENDED':
      case 'NOT_FOUND':
        throw new TaskError('Only ended giveaways can be rerolled.');
      case 'CONFLICT':
        throw new TaskError('That giveaway changed while I was drawing. Please try again.');
      case 'RESOLVE_FAILED':
        throw new TaskError("I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.");
      default:
        throw new TaskError(GENERIC_ERROR);
    }
  }

  // ── giveaway.cancel ─────────────────────────────────────────────────────────

  async function cancel(ctx) {
    const { guild, payload } = ctx;
    await access(ctx);
    const g = await loadGiveaway(guild, giveawayId(payload));
    if (!['ACTIVE', 'DRAFT'].includes(g.status)) throw new TaskError(`That giveaway is ${svc.STATUS_TEXT[g.status]}.`);
    const res = await svc.cancelGiveaway({ guild, giveawayId: g.id });
    if (!res.ok) throw new TaskError('That giveaway already ended or was cancelled.');
    return { message: `Cancelled the giveaway for "${clampText(g.prize, 80)}".`, data: { giveawayId: g.id } };
  }

  // ── giveaway.restart ────────────────────────────────────────────────────────

  async function restart(ctx) {
    const { guild, client, actor, payload } = ctx;
    const settings = await access(ctx);
    const g = await loadGiveaway(guild, giveawayId(payload));
    if (g.status !== 'DRAFT') {
      throw new TaskError('Only draft giveaways can be restarted. A giveaway becomes a draft when its message is deleted.');
    }
    const picked = payload.channelId ?? null;
    if (picked !== null && !isId(picked)) throw invalid();
    const duration = optionalText(payload.duration, 40);
    if (payload.endsAt !== undefined && payload.endsAt !== null && (typeof payload.endsAt !== 'string' || payload.endsAt.length > 40)) {
      throw invalid();
    }

    const channel = await postChannel(guild, actor, picked ?? g.channelId, {
      missing: picked ? 'That channel no longer exists. Pick another one.' : 'The original channel no longer exists. Pick another one.',
    });

    const at = now();
    let endsAt;
    if (duration || payload.endsAt) {
      endsAt = endTime({ duration, endsAt: payload.endsAt ?? null }, settings, at);
    } else if (g.endsAt.getTime() - at.getTime() >= svc.MIN_DURATION_MS) {
      endsAt = g.endsAt;
    } else {
      throw new TaskError("This giveaway's end time has passed. Give it a new length or end time.");
    }

    const res = await svc.restartGiveaway({ client, guild, giveawayId: g.id, channel, endsAt, settings });
    if (res.ok) {
      return {
        message: `"${clampText(g.prize, 80)}" is live again in #${channel.name} and ends ${utc(endsAt)}. Existing entries were kept.`,
        data: { giveawayId: g.id },
      };
    }
    if (res.code === 'NOT_DRAFT' || res.code === 'CONFLICT') {
      throw new TaskError('That giveaway changed meanwhile. Reload the list and try again.');
    }
    throw failStartOrRestart(res, channel);
  }

  // ── giveaway.odds ───────────────────────────────────────────────────────────

  /** Brings the bonus odds to the given list with the same calls /giveaway odds add and remove make. */
  async function odds(ctx) {
    const { guild, payload } = ctx;
    const settings = await access(ctx);
    const g = await loadGiveaway(guild, giveawayId(payload));
    if (!['ACTIVE', 'DRAFT'].includes(g.status)) throw new TaskError('Odds can only be changed while a giveaway is running or in drafts.');

    const wanted = payload.roleWeights;
    if (!Array.isArray(wanted) || wanted.length > svc.MAX_ROLE_WEIGHTS) throw invalid();
    if (!wanted.every(validWeight)) throw invalid();
    if (new Set(wanted.map((w) => w.roleId)).size !== wanted.length) throw new TaskError('Each role can have bonus odds only once.');

    const current = svc.normalizeRoleWeights(g.roleWeights);
    const wantedById = new Map(wanted.map((w) => [w.roleId, w.weight]));
    const removals = current.filter((w) => !wantedById.has(w.roleId));
    const currentById = new Map(current.map((w) => [w.roleId, w.weight]));
    const additions = wanted.filter((w) => currentById.get(w.roleId) !== w.weight);
    for (const w of additions) await pickRole(guild, w.roleId, 'the bonus odds');
    if (!removals.length && !additions.length) return { message: 'The bonus odds are already like that.', data: { giveawayId: g.id } };

    // Removals first, so a full list never hits the cap in the middle.
    for (const w of removals) {
      const res = await svc.updateOdds({ guild, giveawayId: g.id, op: 'remove', roleId: w.roleId, settings });
      if (!res.ok && res.code !== 'NOT_SET') throw oddsFailure(res);
    }
    for (const w of additions) {
      const res = await svc.updateOdds({ guild, giveawayId: g.id, op: 'add', roleId: w.roleId, weight: w.weight, settings });
      if (!res.ok) throw oddsFailure(res);
    }
    return { message: `Updated the bonus odds of "${clampText(g.prize, 80)}".`, data: { giveawayId: g.id } };
  }

  function oddsFailure(res) {
    if (res.code === 'LIMIT') return new TaskError(`A giveaway can have at most ${svc.MAX_ROLE_WEIGHTS} bonus-odds roles.`);
    if (res.code === 'NOT_OPEN') return new TaskError('Odds can only be changed while a giveaway is running or in drafts.');
    return new TaskError('That giveaway no longer exists.');
  }

  return {
    'giveaway.start': { run: start },
    'giveaway.end': { run: end },
    'giveaway.reroll': { run: reroll },
    'giveaway.cancel': { run: cancel },
    'giveaway.restart': { run: restart },
    'giveaway.odds': { run: odds },
  };
}

export const tasks = Object.freeze(createGiveawayTasks());
