// The dashboard's only path to GuildSettings: reads and writes go through settingsService (the single settings
// gateway), stored JSON is read through the bot's own normalisers, and writes repeat the side effects the matching
// slash commands have. What cannot be done without the bot's gateway connection is left to the commands and said so in
// the field descriptions (src/dashboard/fields.js).
// Importing this file opens the Redis connection (through settingsService), so dashboard.js loads it with a dynamic
// import() only once the configuration is valid; tests inject fakes instead.
import { Prisma } from '@prisma/client';
import { getSettings, updateSettingsWith } from '../services/settingsService.js';
import { cache } from '../services/redis.js';
import { cancelJobs } from '../services/scheduler.js';
import { normalizeConfig } from '../services/automodService.js';
import {
  healEmbed, materialiseWelcome, parseCardStyle, serializeCardStyle, welcomeCardEnabled,
} from '../services/welcomeService.js';
import { buildPanelPayload } from '../services/captchaService.js';
import { ensureTickJob } from '../services/birthdayService.js';
import { clampBalancesToCap } from '../services/economyService.js';
import { validateCardBackground as validateWelcomeBackground } from '../utils/welcomeCard.js';
import { validateCardBackground as validateRankBackground } from '../utils/profileCard.js';
import { successEmbed } from '../utils/embedBuilder.js';
import defaultLogger from '../utils/logger.js';
import { FieldError } from './schema.js';
import { JSON_COLUMNS, checkCrossFields, toStoredChanges } from './fields.js';

const voiceSessionKeys = (guildId) => `voice:start:${guildId}:*`;
const BIRTHDAY_SCHEDULE_FIELDS = Object.freeze(['birthdayChannelId', 'birthdayRoleId', 'birthdayHour']);
const CDN = 'https://cdn.discordapp.com';

/**
 * A GuildSettings row with its JSON columns in the shapes src/dashboard/fields.js reads: the automod config upgraded
 * to v2, the welcome embeds healed, the welcome card flag resolved (a legacy `welcomeEmbed.card` included).
 * Never mutates `row`.
 * @param {object|null} row
 * @returns {object|null}
 */
export function normalizeRow(row) {
  if (!row) return row;
  return {
    ...row,
    automodConfig: normalizeConfig(row.automodConfig),
    welcomeCardStyle: serializeCardStyle({ ...parseCardStyle(row.welcomeCardStyle), enabled: welcomeCardEnabled(row) }),
    welcomeEmbed: healEmbed(row.welcomeEmbed),
    welcomeDmEmbed: healEmbed(row.welcomeDmEmbed),
    goodbyeEmbed: healEmbed(row.goodbyeEmbed),
  };
}

/**
 * The guild's current settings, bypassing this process's 30 s memo (the bot may have changed them), normalised.
 * @param {string} guildId
 * @returns {Promise<object>}
 * @throws when Postgres is unreachable
 */
export async function readGuildSettings(guildId) {
  return normalizeRow(await getSettings(guildId, { fresh: true }));
}

/** Null in a Json column is written as Prisma.DbNull (a plain null would store the JSON literal). */
function toDbData(data) {
  const out = { ...data };
  for (const key of JSON_COLUMNS) if (Object.hasOwn(out, key) && out[key] === null) out[key] = Prisma.DbNull;
  return out;
}

/**
 * Downloads a new card background once, as /welcome card and /levelconfig card do, so a broken link is refused.
 * @throws {FieldError}
 */
async function checkBackgrounds(changes, current) {
  const checks = [
    ['welcomeCardStyle', validateWelcomeBackground],
    ['rankCardStyle', validateRankBackground],
  ];
  for (const [field, validate] of checks) {
    const url = changes[field]?.backgroundUrl;
    if (typeof url !== 'string' || url === current?.[field]?.backgroundUrl) continue;
    const result = await validate(url);
    if (!result?.ok) {
      throw new FieldError(`${field}.backgroundUrl`, `That background image cannot be used: ${result?.reason ?? 'it could not be loaded'}`);
    }
  }
}

/** A stand-in for the discord.js Guild that the placeholder renderer reads, from the REST guild object. */
function guildLike(g) {
  return {
    id: g.id,
    name: g.name,
    memberCount: g.memberCount ?? 0,
    ownerId: g.ownerId ?? null,
    premiumTier: g.premiumTier ?? 0,
    premiumSubscriptionCount: g.premiumSubscriptionCount ?? 0,
    verificationLevel: g.verificationLevel ?? 0,
    createdTimestamp: Number((BigInt(g.id) >> 22n) + 1_420_070_400_000n),
    iconURL: () => (g.icon ? `${CDN}/icons/${g.id}/${g.icon}.png?size=256` : null),
  };
}

/** Edits the posted verification panel after its message or button changed (what /captcha message does). */
async function refreshCaptchaPanel(guildId, row, discord) {
  if (!row.captchaChannelId || !row.captchaMessageId) {
    return 'Saved. No verification panel is posted yet: post it with /captcha setup in Discord.';
  }
  try {
    const { guild, channels } = await discord.getGuildResources(guildId);
    const ch = channels.find((c) => c.id === row.captchaChannelId);
    if (!ch) return 'Saved, but the verification channel was deleted: run /captcha setup in Discord.';
    const payload = JSON.parse(JSON.stringify(buildPanelPayload(row, guildLike(guild), { id: ch.id, name: ch.name, type: ch.type })));
    await discord.editMessage(row.captchaChannelId, row.captchaMessageId, {
      content: payload.content ?? '',
      embeds: payload.embeds ?? [],
      components: payload.components ?? [],
      allowed_mentions: { parse: [] },
    });
    return null;
  } catch (err) {
    if (err?.discordCode === 10008) return 'Saved, but the posted panel was deleted: run /captcha setup in Discord to post it again.';
    if (err?.discordCode === 50001 || err?.discordCode === 50013) {
      return 'Saved, but the bot cannot edit the posted panel: check its permissions in the verification channel.';
    }
    return 'Saved, but the posted panel could not be updated right now. Saving again retries.';
  }
}

/**
 * Saves validated changes (fields.validateSettingsChanges; ids already checked against the server) in one row-locked
 * read-modify-write, with the cross-field rules checked against the merged row, then repeats the commands' side effects:
 *   - voice XP or the Levels module switched off: open voice sessions are dropped without credit (/levelconfig);
 *   - the captcha kick timer set to 0: the scheduled kicks are cancelled (/captcha timeout);
 *   - the panel message or Verify button changed: the posted panel is edited (/captcha message);
 *   - birthday channel, role or hour changed: the birthday job is rescheduled (/birthday config);
 *   - the maximum balance lowered: balances above it are lowered (/economy-admin config limits);
 *   - a new moderation log channel: a confirmation is posted there (/modconfig logchannel);
 *   - Auto-Moderation switched on: the stored config is rewritten in the current format (/automod enable);
 *   - the welcome embed changed: the welcome text that was in effect is kept (welcomeService.materialiseWelcome).
 * A failed side effect never undoes the save: it is logged. Side effects that have something to tell the admin (e.g. balances
 * lowered to a new maximum) add a line to `notices`.
 * @param {string} guildId
 * @param {Record<string, unknown>} changes
 * @param {{ discord?: object|null, logger?: object }} [ctx] discord: src/dashboard/discordApi.js (for Discord writes)
 * @returns {Promise<{ row: object, notices: string[] }>} the normalised row after the write
 * @throws {FieldError} a rule of the merged row is broken, or a new card background cannot be loaded (nothing saved)
 * @throws on a database failure (nothing saved)
 */
export async function writeSettings(guildId, changes, { discord = null, logger = defaultLogger } = {}) {
  const stored = await readGuildSettings(guildId);
  await checkBackgrounds(changes, stored);

  let before = stored;
  const written = await updateSettingsWith(guildId, (cur) => {
    const current = normalizeRow(cur);
    before = current;
    const data = toStoredChanges(changes, current);
    if (changes.automodEnabled === true && !Object.hasOwn(data, 'automodConfig')) data.automodConfig = current.automodConfig;
    if (Object.hasOwn(data, 'welcomeEmbed')) materialiseWelcome(cur, data);
    const problem = checkCrossFields({ ...current, ...data }, changes);
    if (problem) throw new FieldError(problem.path, problem.message);
    return toDbData(data);
  });
  const row = normalizeRow(written);

  const notices = [];
  const changed = (field) => Object.hasOwn(changes, field) && JSON.stringify(before?.[field]) !== JSON.stringify(row[field]);
  const effect = async (name, run) => {
    try {
      const notice = await run();
      if (notice) notices.push(notice);
    } catch (err) {
      logger.warn(`Dashboard: ${name} after a settings change in guild ${guildId} failed: ${err?.message ?? err}`);
    }
  };

  if (changes.voiceXpEnabled === false || changes.xpEnabled === false) {
    await effect('dropping voice sessions', () => cache.invalidate(voiceSessionKeys(guildId)).then(() => null));
  }
  if (Object.hasOwn(changes, 'captchaTimeoutMinutes') && row.captchaTimeoutMinutes === 0) {
    await effect('cancelling captcha kicks', async () => {
      const n = await cancelJobs({ guildId, type: 'captcha.timeout' });
      return n > 0 ? `Cancelled ${n} scheduled kick${n === 1 ? '' : 's'} of unverified members.` : null;
    });
  }
  if ((Object.hasOwn(changes, 'captchaTemplate') || Object.hasOwn(changes, 'captchaVerifyButton')) && discord) {
    await effect('refreshing the captcha panel', () => refreshCaptchaPanel(guildId, row, discord));
  }
  if (BIRTHDAY_SCHEDULE_FIELDS.some(changed)) {
    await effect('rescheduling birthdays', () => ensureTickJob(guildId, row, { soon: true }).then(() => null));
  }
  if (changed('economyMaxBalance') && row.economyMaxBalance < before.economyMaxBalance) {
    await effect('lowering balances', async () => {
      const n = await clampBalancesToCap(guildId, row.economyMaxBalance);
      return n > 0 ? `Lowered ${n} balance${n === 1 ? '' : 's'} to the new maximum.` : null;
    });
  }
  if (changed('modLogChannelId') && row.modLogChannelId && discord) {
    await effect('confirming the moderation log', async () => {
      try {
        await discord.sendMessage(row.modLogChannelId, {
          embeds: [successEmbed('Moderation Log', 'Moderation cases will be posted here.').toJSON()],
          allowed_mentions: { parse: [] },
        });
        return null;
      } catch {
        return 'Saved, but the bot cannot post in the moderation log channel: check its permissions there.';
      }
    });
  }
  return { row, notices };
}
