// /economy-boost add | remove | toggle | list (pkg-economy.md amendments item 5, MEE6 Economy > Boosts). A boost multiplies ONE
// /daily or /work payout by (100 + percent) % for a member who holds its role (or for everyone). The period says how often a
// member may use it: one-time (once ever), daily, weekly or monthly (guild timezone). At most LIMITS.ECONOMY_BOOSTS per server.
// Rewards, /pay, casino winnings and admin grants are never boosted. Replies are public; validation errors are private.
import { PermissionFlagsBits, SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  BOOST_PERIOD_LABELS, BOOST_PERCENT_MAX, EconomyError, createBoost, deleteBoost, listBoosts, replyFail, resolveBoost, searchBoosts,
  setBoostEnabled, validateBoostName,
} from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'economy';
export const gated = false; // works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const PER_PAGE = 10;
const PERIOD_CHOICES = Object.freeze({ 'one-time': 'ONE_TIME', daily: 'DAILY', weekly: 'WEEKLY', monthly: 'MONTHLY' });
const PERIOD_TEXT = Object.freeze({
  ONE_TIME: 'once per member, ever',
  DAILY: 'once per member per day',
  WEEKLY: 'once per member per week',
  MONTHLY: 'once per member per month',
});

const nameOpt = (o) => o.setName('name').setDescription('Boost name').setRequired(true).setAutocomplete(true).setMaxLength(32);

export const data = new SlashCommandBuilder()
  .setName('economy-boost')
  .setDescription('Manage coin boosts for /daily and /work (MEE6 boosts, up to 10).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('add').setDescription('Create a coin boost')
    .addStringOption((o) => o.setName('name').setDescription('Boost name (unique)').setRequired(true).setMaxLength(32))
    .addIntegerOption((o) => o.setName('percent').setDescription('Extra coins in percent (1-500)').setRequired(true).setMinValue(1).setMaxValue(BOOST_PERCENT_MAX))
    .addStringOption((o) => o.setName('period').setDescription('How often a member can use it').setRequired(true).addChoices(
      { name: 'One-time', value: 'one-time' }, { name: 'Daily', value: 'daily' }, { name: 'Weekly', value: 'weekly' }, { name: 'Monthly', value: 'monthly' }))
    .addRoleOption((o) => o.setName('role').setDescription('Only members with this role (default: everyone)')))
  .addSubcommand((s) => s.setName('remove').setDescription('Delete a boost').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('toggle').setDescription('Turn a boost on or off').addStringOption(nameOpt))
  .addSubcommand((s) => s.setName('list').setDescription('List the boosts'));

// ── Autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  try {
    const rows = await withTimeout(
      searchBoosts(interaction.guildId, String(interaction.options.getFocused() ?? '')),
      ackBudgetMs(interaction), 'economy-boost autocomplete',
    );
    await respondAutocomplete(interaction, rows.map((row) => ({
      name: `${row.name} (+${row.percent}%)${row.enabled ? '' : ' (off)'}`,
      value: row.id,
    })));
  } catch (err) {
    logger.debug(`economy-boost autocomplete failed: ${err?.message}`);
    await respondAutocomplete(interaction, []);
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const boostLabel = (b) => `**${clampText(escapeMarkdown(b.name), 40)}**`;
const target = (b) => (b.roleId ? `<@&${b.roleId}>` : 'everyone');

async function add(interaction, settings) {
  const checked = validateBoostName(interaction.options.getString('name', true));
  if (!checked.ok) return replyFail(interaction, 'Invalid Name', checked.reason);
  const period = PERIOD_CHOICES[interaction.options.getString('period', true)];
  const role = interaction.options.getRole('role');
  const roleId = role && role.id !== interaction.guildId ? role.id : null; // @everyone means "everyone"
  const percent = interaction.options.getInteger('percent', true);

  let boost;
  try {
    boost = await createBoost(interaction.guildId, { name: checked.value, percent, period, roleId, createdBy: interaction.user.id });
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'INVALID') {
      if (err.data.reason === 'limit') return replyFail(interaction, 'Too Many Boosts', `This server already has ${LIMITS.ECONOMY_BOOSTS} boosts (the maximum).`);
      if (err.data.reason === 'name-taken') return replyFail(interaction, 'Name Taken', `A boost named ${boostLabel({ name: checked.value })} already exists.`);
    }
    throw err;
  }
  logger.info(`Economy boost "${boost.name}" +${percent}% (${period}) created in ${interaction.guildId} by ${interaction.user.id}`);
  return interaction.editReply({
    embeds: [successEmbed('Boost Added',
      `${boostLabel(boost)} gives ${target(boost)} **+${boost.percent}%** on one /daily or /work payout, ${PERIOD_TEXT[boost.period]}.`,
      `${settings.economyEnabled ? '' : 'The economy module is off right now. '}Only the highest unused boost applies to a payout.`)],
  });
}

async function remove(interaction) {
  const value = interaction.options.getString('name', true);
  const boost = await resolveBoost(interaction.guildId, value);
  if (!boost || !(await deleteBoost(interaction.guildId, boost.id))) {
    return replyFail(interaction, 'Boost Not Found', `No boost called **${clampText(escapeMarkdown(value), 40)}** exists.`);
  }
  logger.info(`Economy boost "${boost.name}" deleted in ${interaction.guildId} by ${interaction.user.id}`);
  return interaction.editReply({ embeds: [successEmbed('Boost Removed', `${boostLabel(boost)} was deleted.`)] });
}

async function toggle(interaction) {
  const value = interaction.options.getString('name', true);
  const boost = await resolveBoost(interaction.guildId, value);
  const updated = boost ? await setBoostEnabled(interaction.guildId, boost.id, !boost.enabled) : null;
  if (!updated) return replyFail(interaction, 'Boost Not Found', `No boost called **${clampText(escapeMarkdown(value), 40)}** exists.`);
  return interaction.editReply({
    embeds: [successEmbed(updated.enabled ? 'Boost Enabled' : 'Boost Disabled', `${boostLabel(updated)} is now ${updated.enabled ? 'on' : 'off'}.`)],
  });
}

async function list(interaction, settings) {
  const boosts = await listBoosts(interaction.guildId);
  if (boosts.length === 0) {
    return interaction.editReply({ embeds: [guildEmbed(settings, '🚀 Coin Boosts', 'No boosts yet. Create one with `/economy-boost add`.')] });
  }
  const lines = boosts.map((b) =>
    `${boostLabel(b)} · +${b.percent}% · ${BOOST_PERIOD_LABELS[b.period]} · ${target(b)} · ${b.enabled ? '✅ on' : '⛔ off'} · ${formatNumber(b._count.uses)} use${b._count.uses === 1 ? '' : 's'}`);
  const footer = `${boosts.length}/${LIMITS.ECONOMY_BOOSTS} boosts`;
  const pages = [];
  for (let i = 0; i < lines.length; i += PER_PAGE) {
    pages.push(guildEmbed(settings, '🚀 Coin Boosts', lines.slice(i, i + PER_PAGE).join('\n'), footer));
  }
  return paginate(interaction, pages, { footer: 'append' });
}

export async function execute(interaction) {
  await interaction.deferReply();
  const settings = await getSettings(interaction.guildId);
  switch (interaction.options.getSubcommand()) {
    case 'add': return add(interaction, settings);
    case 'remove': return remove(interaction);
    case 'toggle': return toggle(interaction);
    case 'list': return list(interaction, settings);
    default: return undefined;
  }
}
