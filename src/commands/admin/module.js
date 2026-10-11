import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES, MODULE_NAMES, getModule } from '../../config/modules.js';
import { getSettingsSafe, peekSettings, updateSettings } from '../../services/settingsService.js';
import { withTimeout } from '../../utils/async.js';
import { applyGuildColor, errorEmbed, neutralEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { respondAutocomplete } from '../../utils/interactionHelpers.js';
import { chunkLines } from '../../utils/text.js';
import logger from '../../utils/logger.js';

// Core command: it is how every module is switched on and off (R001, R004, R032, R106), so it
// belongs to no module and can never be gated off itself.
export const module = null;
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('module')
  .setDescription('Turn bot modules on or off for this server.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((sub) => sub
    .setName('list')
    .setDescription('Show every module and whether it is on.'))
  .addSubcommand((sub) => sub
    .setName('enable')
    .setDescription('Turn a module on.')
    .addStringOption((o) => o
      .setName('module')
      .setDescription('The module to turn on')
      .setRequired(true)
      .setAutocomplete(true)))
  .addSubcommand((sub) => sub
    .setName('disable')
    .setDescription('Turn a module off. Its data is kept.')
    .addStringOption((o) => o
      .setName('module')
      .setDescription('The module to turn off')
      .setRequired(true)
      .setAutocomplete(true)));

/** Accepts the module key (what autocomplete submits) or, as a convenience, its label. */
function resolveModuleKey(input) {
  const text = String(input ?? '').trim().toLowerCase();
  if (getModule(text)) return text;
  return MODULE_NAMES.find((k) => MODULES[k].label.toLowerCase() === text) ?? null;
}

export async function execute(interaction) {
  await interaction.deferReply();
  const sub = interaction.options.getSubcommand();
  const guildId = interaction.guildId;

  if (sub === 'list') {
    const settings = await getSettingsSafe(guildId);
    if (!settings) {
      return interaction.editReply({
        embeds: [errorEmbed('Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.')],
      });
    }
    const lines = MODULE_NAMES.map((key) => {
      const mod = MODULES[key];
      const state = settings[mod.flag] === true ? '✅ On' : '❌ Off';
      return `${mod.emoji} **${mod.label}** — ${state} · \`${mod.configCommand}\``;
    });
    const embed = applyGuildColor(
      neutralEmbed('🧩 Server Modules', 'Turn a module on or off with `/module enable` and `/module disable`.'),
      settings,
    );
    // 26 lines do not fit one 1024-character field.
    chunkLines(lines, 1024).forEach((value, i) => {
      embed.addFields({ name: i === 0 ? 'Modules' : 'Modules (continued)', value });
    });
    return interaction.editReply({ embeds: [embed] });
  }

  const enable = sub === 'enable';
  const key = resolveModuleKey(interaction.options.getString('module', true));
  if (!key) {
    return interaction.editReply({
      embeds: [errorEmbed('Unknown Module', 'That is not a module. Use `/module list` to see every module.')],
    });
  }

  const mod = MODULES[key];
  try {
    await updateSettings(guildId, { [mod.flag]: enable });
  } catch (err) {
    logger.error(`/module ${sub} ${key} failed in guild ${guildId}`, err);
    return interaction.editReply({
      embeds: [errorEmbed('Update Failed', 'The setting could not be saved. Please try again in a moment.')],
    });
  }

  return interaction.editReply({
    embeds: [successEmbed(
      enable ? 'Module Enabled' : 'Module Disabled',
      enable
        ? `**${mod.label}** enabled. Configure it with \`${mod.configCommand}\`.`
        : `**${mod.label}** disabled. Its data is kept.`,
    )],
  });
}

export async function autocomplete(interaction) {
  const guildId = interaction.guildId;
  // Memo first; a cold load is bounded well inside the 3-second answer window.
  const settings = (guildId && (peekSettings(guildId)
    ?? await withTimeout(getSettingsSafe(guildId), 1000, 'module autocomplete').catch(() => null))) || null;

  const sub = interaction.options.getSubcommand(false);
  const query = String(interaction.options.getFocused() ?? '').trim().toLowerCase();
  const isOn = (key) => settings?.[MODULES[key].flag] === true;

  let keys = MODULE_NAMES.filter((k) => !query
    || k.includes(query)
    || MODULES[k].label.toLowerCase().includes(query));

  // Discord shows at most 25 of the 26 modules: offer the ones the admin can act on first.
  if (settings && (sub === 'enable' || sub === 'disable')) {
    const actionable = (k) => (sub === 'enable' ? !isOn(k) : isOn(k));
    keys = [...keys.filter(actionable), ...keys.filter((k) => !actionable(k))];
  }

  await respondAutocomplete(interaction, keys.slice(0, 25).map((key) => {
    const mod = MODULES[key];
    const state = settings ? (isOn(key) ? ' — on' : ' — off') : '';
    return { name: `${mod.emoji} ${mod.label} (${key})${state}`, value: key };
  }));
}
