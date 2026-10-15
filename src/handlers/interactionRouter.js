import { PERMISSION_LEVELS } from '../config/constants.js';
import { prefixInfo, parseCustomId } from '../config/customIds.js';
import { MODULES, isModuleEnabled } from '../config/modules.js';
import { getSettingsSafe, peekSettings } from '../services/settingsService.js';
import { withTimeout } from '../utils/async.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { errorEmbed, warnEmbed } from '../utils/embedBuilder.js';
import logger from '../utils/logger.js';

// How long the router may wait for a cold settings load before it gives up. Commands keep at least
// ~1 s of Discord's 3-second window; they bound their own pre-acknowledgement lookups with ackBudgetMs.
const ROUTER_SETTINGS_TIMEOUT_MS = 1500;

// Autocomplete must answer inside the same 3 s window and the handler behind it usually queries the database,
// so its gate gets a tighter settings budget than a slash command (which defers before doing real work).
const AUTOCOMPLETE_SETTINGS_TIMEOUT_MS = 1000;

// A collector-only prefix (rps, ttt, bj, paginator, ...) has no registered handler: its live collector
// answers the click. If nothing answered after this delay the collector is gone.
const COLLECTOR_GRACE_MS = 2500;

// ── Safe reply helper — works whether or not we've already replied ─────────────

async function safeReply(interaction, payload) {
  try {
    if (!interaction.replied && !interaction.deferred) {
      return await interaction.reply({ ...payload, ephemeral: true });
    }
    // Only deferReply() leaves a boolean `ephemeral`; it is the one state where @original is a placeholder
    // for this answer. After deferUpdate()/update() (ephemeral null) @original is the message that was clicked
    // (a public panel), and after an earlier reply it is that reply: never overwrite either with an error.
    if (interaction.deferred && !interaction.replied && typeof interaction.ephemeral === 'boolean') {
      return await interaction.editReply(payload);
    }
    return await interaction.followUp({ ...payload, ephemeral: true });
  } catch {
    // Interaction may have expired — silently discard
  }
}

// ── Component routing ─────────────────────────────────────────────────────────
// CustomIDs follow the format "prefix:param1:param2"; the prefix picks the handler.
// Persistent prefixes are registered at startup (plugins, core, legacy registerHandlers).

const NO_HANDLER_TEXT = {
  button: 'This button is no longer active.',
  select: 'This menu is no longer active.',
  modal:  'This form has expired. Please run the command again.',
};

const ERROR_EMBED = {
  button: ['Button Error', 'Something went wrong processing this button.'],
  select: ['Menu Error',   'Something went wrong processing this selection.'],
  modal:  ['Modal Error',  'Something went wrong processing your submission.'],
};

async function routeComponent(interaction, client, kind, handlers) {
  const { prefix } = parseCustomId(interaction.customId);
  const handler = handlers?.get(prefix);

  if (!handler) {
    if (kind !== 'modal' && prefixInfo(prefix)?.kind === 'collector') {
      // Give the live collector the first chance to answer (R272); only a click nobody handled
      // after the grace period gets the expiry notice.
      const timer = setTimeout(() => {
        try {
          if (!interaction.replied && !interaction.deferred) {
            interaction.reply({ content: 'This menu has expired.', ephemeral: true }).catch(() => {});
          }
        } catch {
          // ignore
        }
      }, COLLECTOR_GRACE_MS);
      timer.unref?.();
      return;
    }
    await interaction.reply({ content: NO_HANDLER_TEXT[kind], ephemeral: true }).catch(() => {});
    return;
  }

  try {
    await handler(interaction, client);
  } catch (err) {
    logger.error(`${kind[0].toUpperCase()}${kind.slice(1)} handler error for prefix "${prefix}"`, err);
    await safeReply(interaction, { embeds: [errorEmbed(...ERROR_EMBED[kind])] });
  }
}

// ── Access gate (slash commands and their autocomplete) ───────────────────────

/**
 * The one gate every command invocation passes: the guild guard, the required permission level (a
 * subcommand or group + subcommand entry beats the command-wide level) and the module toggle. It sends
 * nothing itself. A refusal carries the embed a slash command answers with; autocomplete answers an empty
 * list instead. Autocomplete must be gated exactly like execute, or it leaks admin-only data (role, channel
 * and saved-item names) to anyone who can type the command.
 *
 * @param {import('discord.js').ChatInputCommandInteraction|import('discord.js').AutocompleteInteraction} interaction
 * @param {object} command
 * @param {number} settingsTimeoutMs how long a cold settings load may take
 * @returns {Promise<{ ok: true, settings: object|null } | { ok: false, embed: import('discord.js').EmbedBuilder }>}
 */
async function checkCommandAccess(interaction, command, settingsTimeoutMs) {
  // Guild guard. Fails closed: a permission level can only be resolved from a cached guild
  // member, so a command that is not explicitly DM-safe never runs without one (R123, R124, R271).
  if (command.allowInDMs !== true) {
    if (!interaction.inGuild()) {
      return { ok: false, embed: errorEmbed('Server Only', 'This command can only be used in a server.') };
    }
    if (!interaction.inCachedGuild()) {
      return {
        ok: false,
        embed: errorEmbed('Bot Not In Server', 'I need to be a member of this server to run commands here. Ask an admin to invite me with the bot scope.'),
      };
    }
  }

  // Required level: a subcommand (or group + subcommand) entry beats the command-wide level.
  const sub   = interaction.options.getSubcommand(false);
  const group = interaction.options.getSubcommandGroup(false);
  const perms = command.subcommandPermissions;
  const required =
    (sub && group ? perms?.[`${group} ${sub}`] : undefined) ??
    (sub ? perms?.[sub] : undefined) ??
    command.permissionLevel ??
    PERMISSION_LEVELS.MEMBER;

  // A level above MEMBER needs a resolvable member; a DM-safe command used outside a cached guild
  // cannot prove one.
  if (required > PERMISSION_LEVELS.MEMBER && !interaction.inCachedGuild()) {
    return { ok: false, embed: errorEmbed('Server Only', 'This command can only be used in a server where I am a member.') };
  }

  const gated = Boolean(command.module) && command.gated !== false;
  let settings = null;
  if (interaction.guildId && (required > PERMISSION_LEVELS.MEMBER || gated)) {
    // Memo first (no I/O), then a bounded load so a slow Redis or Postgres cannot eat the 3 s window (R125).
    settings = peekSettings(interaction.guildId)
      ?? await withTimeout(getSettingsSafe(interaction.guildId), settingsTimeoutMs, 'router settings').catch(() => null);
  }

  if (gated && interaction.guildId) {
    if (settings === null) {
      return { ok: false, embed: errorEmbed('Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.') };
    }
    if (!isModuleEnabled(settings, command.module)) {
      const label = MODULES[command.module]?.label ?? command.module;
      return {
        ok: false,
        embed: warnEmbed('Module Disabled', `The **${label}** module is turned off on this server. An admin can enable it with \`/module enable ${command.module}\`.`),
      };
    }
  }

  if (required > PERMISSION_LEVELS.MEMBER) {
    // With settings === null a moderator role cannot be proven, but Discord permissions still count.
    const memberLevel = resolvePermissionLevel(interaction.member, settings);
    if (memberLevel < required) {
      return { ok: false, embed: errorEmbed('Insufficient Permissions', "You don't have permission to use this command.") };
    }
  }

  return { ok: true, settings };
}

// ── Main router ───────────────────────────────────────────────────────────────

/**
 * Routes any Discord interaction to the appropriate handler.
 * Called from src/events/interactionCreate.js.
 *
 * @param {import('discord.js').Interaction} interaction
 * @param {import('discord.js').Client}      client
 */
export async function routeInteraction(interaction, client) {

  // ── Slash Commands ──────────────────────────────────────────────────────────
  if (interaction.isChatInputCommand()) {
    const command = client.commands.get(interaction.commandName);
    if (!command) {
      return safeReply(interaction, {
        embeds: [errorEmbed('Unknown Command', 'This command no longer exists. Try `/` to see available commands.')],
      });
    }

    const access = await checkCommandAccess(interaction, command, ROUTER_SETTINGS_TIMEOUT_MS);
    if (!access.ok) return safeReply(interaction, { embeds: [access.embed] });

    try {
      await command.execute(interaction, client);
    } catch (err) {
      logger.error(`Error in command "${interaction.commandName}"`, err, {
        guildId: interaction.guildId,
        userId:  interaction.user.id,
      });
      await safeReply(interaction, {
        embeds: [errorEmbed('Command Error', 'Something went wrong while running this command.')],
      });
    }
    return;
  }

  // ── Autocomplete ────────────────────────────────────────────────────────────
  if (interaction.isAutocomplete()) {
    const command = client.commands.get(interaction.commandName);
    if (!command?.autocomplete) return;
    // Gated exactly like execute (guild guard, permission level incl. subcommandPermissions, module toggle).
    // Anything short of a clean pass answers with no suggestions: never an error, never data.
    let access;
    try {
      access = await checkCommandAccess(interaction, command, AUTOCOMPLETE_SETTINGS_TIMEOUT_MS);
    } catch (err) {
      logger.error(`Autocomplete gate error for "${interaction.commandName}"`, err);
      access = { ok: false };
    }
    if (!access.ok) {
      await interaction.respond([]).catch(() => {});
      return;
    }
    try {
      await command.autocomplete(interaction, client);
    } catch (err) {
      logger.error(`Autocomplete error for "${interaction.commandName}"`, err);
    }
    return;
  }

  // ── Buttons, select menus (every kind) and modal submits ────────────────────
  if (interaction.isButton()) return routeComponent(interaction, client, 'button', client.buttonHandlers);
  if (interaction.isAnySelectMenu()) return routeComponent(interaction, client, 'select', client.selectHandlers);
  if (interaction.isModalSubmit()) return routeComponent(interaction, client, 'modal', client.modalHandlers);
}
