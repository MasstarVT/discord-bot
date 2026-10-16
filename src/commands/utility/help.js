import { SlashCommandBuilder } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES, MODULE_NAMES, getModule, isModuleEnabled } from '../../config/modules.js';
import prisma from '../../database/client.js';
import { buildCommandPayload } from '../../handlers/commandLoader.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { withTimeout } from '../../utils/async.js';
import { applyGuildColor, neutralEmbed } from '../../utils/embedBuilder.js';
import { respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { chunkLines, clampText } from '../../utils/text.js';

// Core command: never gated, always visible.
export const module = null;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('help')
  .setDescription('Show the commands you can use here.')
  .addStringOption((o) => o
    .setName('command')
    .setDescription('Show details for one command')
    .setAutocomplete(true));

// Labels of the core (module-less) commands, by folder name. Anything else is "Other".
const CATEGORY_LABELS = {
  utility:    '🔧 Utility',
  admin:      '⚙️ Admin',
  moderation: '🔨 Moderation',
  community:  '👥 Community',
  economy:    '🪙 Economy',
  games:      '🎮 Games',
  leveling:   '⭐ Levels',
  search:     '🔎 Search',
};
const OTHER_LABEL = '📦 Other';
const SETUP_LABEL = '⚙️ Setup (modules currently off)';
const CUSTOM_LABEL = '🏷️ Custom Commands';

const LEVEL_NAMES = {
  [PERMISSION_LEVELS.MEMBER]:    'Member',
  [PERMISSION_LEVELS.MODERATOR]: 'Moderator',
  [PERMISSION_LEVELS.ADMIN]:     'Admin',
  [PERMISSION_LEVELS.OWNER]:     'Owner',
};

// Discord caps an embed at 25 fields and 6000 characters in total; stay clear of both.
const MAX_FIELDS = 25;
const MAX_CHARS = 5500;
const FIELD_MAX = 1024;
const SUB_COMMAND = 1;
const SUB_COMMAND_GROUP = 2;

// ── Visibility ────────────────────────────────────────────────────────────────

/** The lowest level that can run any part of the command. */
function minRequiredLevel(mod) {
  return Math.min(
    mod.permissionLevel ?? PERMISSION_LEVELS.MEMBER,
    ...Object.values(mod.subcommandPermissions ?? {}),
  );
}

/** The command's module key, or null for a core command (including an unknown key). */
function moduleKeyOf(mod) {
  return getModule(mod.module) ? mod.module : null;
}

/**
 * 'show' (listed normally), 'setup' (a config command of a module that is off, shown to admins
 * only) or 'hide'. With no settings the module state is unknown, so nothing is hidden for it.
 */
function visibility(mod, userLevel, settings) {
  if (userLevel < minRequiredLevel(mod)) return 'hide';
  const key = moduleKeyOf(mod);
  if (!key) return 'show';
  const on = settings ? isModuleEnabled(settings, key) : true;
  if (on) return 'show';
  if (mod.gated === false) return userLevel >= PERMISSION_LEVELS.ADMIN ? 'setup' : 'hide';
  return 'hide';
}

async function loadSettings(guildId, timeoutMs) {
  return peekSettings(guildId)
    ?? await withTimeout(getSettingsSafe(guildId), timeoutMs, 'help settings').catch(() => null);
}

// ── The command list ──────────────────────────────────────────────────────────

const byName = (a, b) => a.name.localeCompare(b.name);

/** Groups the commands this user may see: module groups, core groups, then the setup group. */
function buildGroups(client, userLevel, settings) {
  const modules = new Map();
  const categories = new Map();
  const setup = [];
  let count = 0;

  for (const [name, mod] of client.commands) {
    const verdict = visibility(mod, userLevel, settings);
    if (verdict === 'hide') continue;
    count++;
    const entry = { name, line: `\`/${name}\` — ${clampText(mod.data.description ?? '', 100)}` };
    if (verdict === 'setup') {
      setup.push(entry);
    } else if (moduleKeyOf(mod)) {
      const list = modules.get(mod.module) ?? [];
      list.push(entry);
      modules.set(mod.module, list);
    } else {
      const category = client.commandCategories?.get(name) ?? 'general';
      const list = categories.get(category) ?? [];
      list.push(entry);
      categories.set(category, list);
    }
  }

  const groups = [];
  const add = (label, entries) => {
    if (entries?.length) groups.push({ label, lines: [...entries].sort(byName).map((e) => e.line) });
  };
  for (const key of MODULE_NAMES) add(`${MODULES[key].emoji} ${MODULES[key].label}`, modules.get(key));
  for (const [category, label] of Object.entries(CATEGORY_LABELS)) add(label, categories.get(category));
  add(OTHER_LABEL, [...categories].filter(([c]) => !(c in CATEGORY_LABELS)).flatMap(([, e]) => e));
  add(SETUP_LABEL, setup);
  return { groups, count };
}

/**
 * The prefix custom commands answer to, by the same rule as the custom-commands module (`effectivePrefix` in
 * tagService): '' means prefix invocation is off (null), a valid prefix (1-5 characters, no whitespace) is itself,
 * anything else (a hand-edited row) is '!'. Duplicated here because core must not import a feature package.
 * @param {unknown} p GuildSettings.customPrefix
 * @returns {string|null}
 */
export function customPrefixOf(p) {
  if (p === '') return null;
  return typeof p === 'string' && p.length >= 1 && [...p].length <= 5 && !/\s/u.test(p) ? p : '!';
}

/**
 * The /help line of one custom command: `!name` while prefix commands are on, `/tag use name:<x>` when the
 * prefix is off (`/settings prefix off`), because a bare name would not run anything then.
 * @param {{ name: string, description?: string|null }} tag
 * @param {string|null} prefix customPrefixOf(settings.customPrefix); null = prefix invocation is off
 * @returns {string}
 */
export function customCommandLine(tag, prefix) {
  const usage = prefix === null ? `/tag use name:${tag.name}` : `${prefix}${tag.name}`;
  return `\`${usage}\` — ${clampText(tag.description ?? 'custom command', 100)}`;
}

/**
 * Whether a member's roles may run a custom command: the role half of `checkTagAccess` in tagService, duplicated
 * here because core must not import a feature package.
 * @param {{ allowRoleIds?: string[], denyRoleIds?: string[] }} tag
 * @param {import('discord.js').GuildMember|null} member
 */
export function tagRolesAllow(tag, member) {
  const roles = member?.roles?.cache;
  const allow = tag?.allowRoleIds ?? [];
  if ((tag?.denyRoleIds ?? []).some((id) => roles?.has(id))) return false;
  return !allow.length || allow.some((id) => roles?.has(id));
}

/**
 * Read-only look at the guild's enabled custom commands (00-overview §10 allows reading CustomTag).
 * `viewer` (a non-admin member) hides the commands their roles cannot run, as /tag list does.
 */
export async function customCommandGroup(guildId, settings, viewer = null) {
  if (!settings || !isModuleEnabled(settings, 'customcommands')) return null;
  const prefix = customPrefixOf(settings.customPrefix);
  try {
    const found = await withTimeout(prisma.customTag.findMany({
      where: { guildId, enabled: true },
      orderBy: [{ uses: 'desc' }, { name: 'asc' }],
      take: viewer ? LIMITS.CUSTOM_TAGS : 25,
      select: { name: true, description: true, allowRoleIds: true, denyRoleIds: true },
    }), 3000, 'help custom commands');
    const tags = (viewer ? found.filter((t) => tagRolesAllow(t, viewer)) : found).slice(0, 25);
    if (!tags.length) return null;
    return {
      label: CUSTOM_LABEL,
      lines: tags.map((t) => customCommandLine(t, prefix)),
    };
  } catch (err) {
    logger.debug(`/help: could not read custom commands: ${err?.message}`);
    return null;
  }
}

/** Splits the groups into embeds that respect the field and character limits. */
function buildPages(groups, settings, userLevel, count) {
  const title = '📖 Commands';
  const description = `Your access level: **${LEVEL_NAMES[userLevel] ?? 'Member'}** · Details: \`/help command:<name>\``;
  const footer = `${count} command${count === 1 ? '' : 's'} available to you`;
  const overhead = title.length + description.length + footer.length + 40; // room for the page indicator

  const fields = groups.flatMap((g) => chunkLines(g.lines, FIELD_MAX).map((value, i) => ({
    name: i === 0 ? g.label : `${g.label} (cont.)`,
    value,
  })));

  const pages = [];
  let current = [];
  let size = overhead;
  for (const field of fields) {
    const cost = field.name.length + field.value.length;
    if (current.length && (current.length >= MAX_FIELDS || size + cost > MAX_CHARS)) {
      pages.push(current);
      current = [];
      size = overhead;
    }
    current.push(field);
    size += cost;
  }
  if (current.length) pages.push(current);

  return pages.map((pageFields) => applyGuildColor(
    neutralEmbed(title, description, footer).addFields(pageFields),
    settings,
  ));
}

// ── /help command:<name> ──────────────────────────────────────────────────────

function levelBadge(level, base) {
  return level > base ? ` *(${LEVEL_NAMES[level] ?? level}+)*` : '';
}

function subcommandLines(mod, json) {
  const lines = [];
  const perms = mod.subcommandPermissions ?? {};
  const base = mod.permissionLevel ?? PERMISSION_LEVELS.MEMBER;
  for (const opt of json.options ?? []) {
    if (opt.type === SUB_COMMAND) {
      const level = perms[opt.name] ?? base;
      lines.push(`\`${opt.name}\` — ${opt.description}${levelBadge(level, PERMISSION_LEVELS.MEMBER)}`);
    } else if (opt.type === SUB_COMMAND_GROUP) {
      for (const sub of opt.options ?? []) {
        const level = perms[`${opt.name} ${sub.name}`] ?? perms[sub.name] ?? base;
        lines.push(`\`${opt.name} ${sub.name}\` — ${sub.description}${levelBadge(level, PERMISSION_LEVELS.MEMBER)}`);
      }
    }
  }
  return lines;
}

function usageOf(name, json) {
  const options = (json.options ?? []).filter((o) => o.type !== SUB_COMMAND && o.type !== SUB_COMMAND_GROUP);
  return `/${name}${options.map((o) => (o.required ? ` <${o.name}>` : ` [${o.name}]`)).join('')}`;
}

async function showCommand(interaction, client, wanted, userLevel, settings) {
  const name = wanted.trim().replace(/^\//, '').toLowerCase();
  const mod = client.commands.get(name);
  if (!mod || visibility(mod, userLevel, settings) === 'hide') {
    return interaction.editReply({ content: 'No such command.' });
  }

  const json = buildCommandPayload(mod);
  const key = moduleKeyOf(mod);
  const moduleText = key
    ? `${MODULES[key].emoji} ${MODULES[key].label}${settings ? (isModuleEnabled(settings, key) ? ' — on' : ' — off') : ''}`
    : 'Core (always available)';

  const embed = applyGuildColor(neutralEmbed(`/${name}`, clampText(mod.data.description ?? '', 4000)), settings)
    .addFields(
      { name: 'Usage', value: `\`${clampText(usageOf(name, json), 200)}\``, inline: false },
      { name: 'Module', value: moduleText, inline: true },
      { name: 'Required level', value: LEVEL_NAMES[mod.permissionLevel ?? PERMISSION_LEVELS.MEMBER] ?? 'Member', inline: true },
    );

  const subs = subcommandLines(mod, json);
  chunkLines(subs, FIELD_MAX).slice(0, 10).forEach((value, i) => {
    embed.addFields({ name: i === 0 ? 'Subcommands' : 'Subcommands (cont.)', value });
  });

  return interaction.editReply({ embeds: [embed] });
}

// ── Entry points ──────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const settings = await getSettingsSafe(interaction.guildId);
  const userLevel = resolvePermissionLevel(interaction.member, settings);

  const wanted = interaction.options.getString('command');
  if (wanted) return showCommand(interaction, client, wanted, userLevel, settings);

  const { groups, count } = buildGroups(client, userLevel, settings);
  const custom = await customCommandGroup(interaction.guildId, settings, userLevel < PERMISSION_LEVELS.ADMIN ? interaction.member : null);
  if (custom) {
    // Custom commands sit before the setup group, which always comes last.
    const setupAt = groups.findIndex((g) => g.label === SETUP_LABEL);
    groups.splice(setupAt === -1 ? groups.length : setupAt, 0, custom);
  }

  if (groups.length === 0) {
    return interaction.editReply({ content: 'No commands are available to you.' });
  }

  const pages = buildPages(groups, settings, userLevel, count + (custom ? custom.lines.length : 0));
  await paginate(interaction, pages, { ephemeral: true });
}

export async function autocomplete(interaction, client) {
  // Memo first; a cold load is bounded well inside the 3-second answer window.
  const settings = await loadSettings(interaction.guildId, 1000);
  const userLevel = resolvePermissionLevel(interaction.member, settings);
  const query = String(interaction.options.getFocused() ?? '').trim().replace(/^\//, '').toLowerCase();

  const choices = [...client.commands]
    .filter(([name, mod]) => (!query || name.includes(query)) && visibility(mod, userLevel, settings) !== 'hide')
    .map(([name]) => name)
    .sort((a, b) => a.localeCompare(b))
    .slice(0, 25)
    .map((name) => ({ name: `/${name}`, value: name }));

  await respondAutocomplete(interaction, choices);
}
