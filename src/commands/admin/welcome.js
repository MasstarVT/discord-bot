import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  WelcomeConfigError, addAutorole, autoroleListLines, buildSetupModal, buildStatusEmbed, buildTestPreview,
  cardChannelWarning, cardSummary, checkConfigChannel, delayText, describeEmbedSettings, diagnoseWelcome,
  autoroleNotes, messageInfoEmbed, messageUpdateEmbed, pruneMissingAutoroles, removeAutorole,
  resolveAutoroles, resolveTemplate, saveSettings, setAutoroleDelay, settingsForAck, updateCardStyle,
  updateEmbedOptions, updateMessage, welcomeChannelNeeds, withCardPreview,
} from '../../services/welcomeService.js';
import { getSettings, getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { applyGuildColor, errorEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { respondAutocomplete } from '../../utils/interactionHelpers.js';
import { joinWithLimit } from '../../utils/text.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';

export const module = 'welcome';
export const gated = false; // configuration works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const TXT = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const thumb = (o) => o.setName('thumbnail').setDescription('Small corner image')
  .addChoices({ name: 'Member avatar', value: 'avatar' }, { name: 'Server icon', value: 'server' }, { name: 'None', value: 'none' });

export const data = new SlashCommandBuilder()
  .setName('welcome')
  .setDescription('Welcome & Goodbye: join message, card, DM and autoroles (MEE6 Welcome & Goodbye).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Turn the Welcome & Goodbye module on.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn the Welcome & Goodbye module off (settings are kept).'))
  .addSubcommand((s) => s.setName('status').setDescription('Show all welcome, DM, goodbye and autorole settings.'))
  .addSubcommand((s) => s.setName('channel').setDescription('Set the channel for welcome messages.')
    .addChannelOption((o) => o.setName('channel').setDescription('Welcome channel').addChannelTypes(...TXT))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('message').setDescription('Set the welcome text; {user} pings the new member.')
    .addStringOption((o) => o.setName('text').setDescription('Text with variables such as {user} and {server}').setMaxLength(2000))
    .addBooleanOption((o) => o.setName('enabled').setDescription('Post the welcome message in the channel'))
    .addBooleanOption((o) => o.setName('reset').setDescription('Use the default text')))
  .addSubcommand((s) => s.setName('embed').setDescription('Send text only or text + embed; set embed images.')
    .addBooleanOption((o) => o.setName('enabled').setDescription('true = text + embed, false = text only'))
    .addStringOption(thumb)
    .addStringOption((o) => o.setName('image_url').setDescription('Large image URL, {user.avatar}, or none').setMaxLength(2048))
    .addBooleanOption((o) => o.setName('reset').setDescription('Use the default embed')))
  .addSubcommand((s) => s.setName('setup').setDescription('Edit the welcome text and embed in a form.'))
  .addSubcommand((s) => s.setName('card').setDescription('Welcome card image: on/off and style.')
    .addBooleanOption((o) => o.setName('enabled').setDescription('Attach the welcome card'))
    .addStringOption((o) => o.setName('background').setDescription('Background colour, e.g. #23272A').setMaxLength(16))
    .addStringOption((o) => o.setName('background_url').setDescription('Background image URL, or none').setMaxLength(2048))
    .addStringOption((o) => o.setName('text_color').setDescription('Text colour, e.g. #FFFFFF').setMaxLength(16))
    .addStringOption((o) => o.setName('accent_color').setDescription('Accent colour, e.g. #5865F2').setMaxLength(16))
    .addNumberOption((o) => o.setName('overlay').setDescription('Darkening over the background image, 0 to 1').setMinValue(0).setMaxValue(1))
    .addBooleanOption((o) => o.setName('reset').setDescription('Reset the card style')))
  .addSubcommand((s) => s.setName('test').setDescription('Preview the welcome as if you just joined.')
    .addStringOption((o) => o.setName('type').setDescription('What to preview')
      .addChoices({ name: 'Channel message', value: 'channel' }, { name: 'Welcome DM', value: 'dm' })))
  .addSubcommandGroup((g) => g.setName('dm').setDescription('Welcome DM sent to new members.')
    .addSubcommand((s) => s.setName('enable').setDescription('DM new members when they join.'))
    .addSubcommand((s) => s.setName('disable').setDescription('Stop the welcome DM.'))
    .addSubcommand((s) => s.setName('message').setDescription('Set the DM text.')
      .addStringOption((o) => o.setName('text').setDescription('Text with variables such as {server}').setMaxLength(2000))
      .addBooleanOption((o) => o.setName('reset').setDescription('Use the default text')))
    .addSubcommand((s) => s.setName('setup').setDescription('Edit the DM text and embed in a form.'))
    .addSubcommand((s) => s.setName('card').setDescription('Attach the welcome card to the DM.')
      .addBooleanOption((o) => o.setName('enabled').setDescription('Attach the card').setRequired(true))))
  .addSubcommandGroup((g) => g.setName('autorole').setDescription('Roles given to new members (MEE6 welcome role).')
    .addSubcommand((s) => s.setName('add').setDescription('Add a role given to new members.')
      .addRoleOption((o) => o.setName('role').setDescription('Role to give'))
      .addStringOption((o) => newRoleOption(o))
      .addStringOption((o) => o.setName('delay').setDescription('Delay for all autoroles, e.g. 10m (0 = at once, max 7d)').setMaxLength(32)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove an autorole, including deleted roles.')
      .addStringOption((o) => o.setName('role').setDescription('Autorole to remove').setRequired(true).setAutocomplete(true)))
    .addSubcommand((s) => s.setName('delay').setDescription('Set the delay before autoroles are given.')
      .addStringOption((o) => o.setName('duration').setDescription('e.g. 10m or 2h; 0 = at once; max 7d').setRequired(true).setMaxLength(32)))
    .addSubcommand((s) => s.setName('list').setDescription('List the autoroles and the delay.')));

const unavailable = () => errorEmbed('Temporarily Unavailable', 'Settings could not be loaded. Try again in a moment.');
const PREVIEW_KINDS = Object.freeze({ channel: 'welcome', dm: 'dm' });

// ── Autocomplete ───────────────────────────────────────────────────────────────

/** `/welcome autorole remove role`: the configured autoroles, deleted ones included. */
export async function autocomplete(interaction) {
  const focused = interaction.options.getFocused(true);
  if (focused.name !== 'role' || interaction.options.getSubcommand(false) !== 'remove') {
    return respondAutocomplete(interaction, []);
  }
  const settings = peekSettings(interaction.guildId)
    ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'welcome autocomplete').catch(() => null);
  const query = String(focused.value ?? '').toLowerCase();
  const choices = (settings?.autoroleIds ?? [])
    .map((id) => {
      const role = interaction.guild?.roles.cache.get(id);
      return { name: role ? `@${role.name}` : `Deleted role (${id})`, value: id };
    })
    .filter((c) => c.name.toLowerCase().includes(query) || c.value.includes(query))
    .slice(0, 25);
  return respondAutocomplete(interaction, choices);
}

// ── Execute ────────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  const { guildId } = interaction;

  // Forms and previews are acknowledged by showModal / buildTestPreview themselves, so settings come
  // from the memo or a bounded lookup (rule 21) before any defer.
  if (key === 'setup' || key === 'dm setup' || key === 'test') {
    const settings = await settingsForAck(interaction);
    if (!settings) return interaction.reply({ embeds: [unavailable()], ephemeral: true });
    if (key === 'test') {
      const type = interaction.options.getString('type') ?? 'channel';
      return buildTestPreview(interaction, settings, PREVIEW_KINDS[type] ?? 'welcome');
    }
    return interaction.showModal(buildSetupModal(key === 'setup' ? 'welcome' : 'dm', settings));
  }

  await interaction.deferReply();
  try {
    let settings;
    try {
      settings = await getSettings(guildId);
    } catch (err) {
      logger.warn(`/welcome could not load settings for ${guildId}: ${err?.message}`);
      return await interaction.editReply({ embeds: [unavailable()] });
    }
    return await run(interaction, key, settings);
  } catch (err) {
    if (err instanceof WelcomeConfigError) {
      return interaction.editReply({ embeds: [errorEmbed(err.title ?? 'Not Saved', err.message)] });
    }
    throw err;
  }
}

async function run(interaction, key, s) {
  const { guild, guildId } = interaction;
  const reply = (embed, extra = {}) => interaction.editReply({ embeds: [embed], ...extra });
  if (!(await resolveNewOptions(interaction, data, { required: ['channel', 'role'] }))) return;

  switch (key) {
    case 'enable': {
      const after = await saveSettings(guildId, { welcomeEnabled: true });
      const problems = diagnoseWelcome(guild, after, 'welcome');
      const tail = problems.length ? problems.join('\n') : 'Preview it with `/welcome test`.';
      return reply(successEmbed('Welcome & Goodbye Enabled', `Welcome messages, welcome DMs and goodbye messages are on.\n${tail}`));
    }

    case 'disable':
      await saveSettings(guildId, { welcomeEnabled: false });
      return reply(successEmbed('Welcome & Goodbye Disabled',
        'Welcome messages, welcome DMs and goodbye messages are off. Autoroles keep working; remove them with `/welcome autorole remove`.'));

    case 'status':
      return reply(buildStatusEmbed(guild, s));

    case 'channel': {
      const picked = interaction.options.getChannel('channel', true);
      const check = await checkConfigChannel(guild, picked, welcomeChannelNeeds(s));
      if (!check.ok) return reply(errorEmbed('Cannot Use That Channel', check.message));
      const after = await saveSettings(guildId, { welcomeChannelId: picked.id });
      const lines = [`Welcome messages will be posted in <#${picked.id}>.`];
      if (!after.welcomeEnabled) lines.push('⚠️ The Welcome & Goodbye module is off (`/welcome enable`).');
      return reply(successEmbed('Welcome Channel Set', lines.join('\n')));
    }

    case 'message':
    case 'dm message': {
      const kind = key === 'message' ? 'welcome' : 'dm';
      const text = interaction.options.getString('text');
      const reset = interaction.options.getBoolean('reset') === true;
      const enabled = kind === 'welcome' ? interaction.options.getBoolean('enabled') : null;
      if (text === null && !reset && enabled === null) {
        return reply(messageInfoEmbed(kind === 'welcome' ? 'Welcome Text' : 'Welcome DM Text', kind, s));
      }
      const after = await updateMessage(guildId, kind, { text, reset, enabled });
      const ctx = { member: interaction.member, guild, channel: interaction.channel };
      return reply(messageUpdateEmbed(kind === 'welcome' ? 'Welcome Text Updated' : 'Welcome DM Text Updated', kind, after, { text, reset, enabled }, ctx));
    }

    case 'embed': {
      const after = await updateEmbedOptions(guildId, 'welcome', {
        enabled: interaction.options.getBoolean('enabled'),
        thumbnail: interaction.options.getString('thumbnail'),
        imageUrl: interaction.options.getString('image_url'),
        reset: interaction.options.getBoolean('reset') === true,
      });
      return reply(successEmbed('Welcome Embed Updated', describeEmbedSettings('welcome', after)));
    }

    case 'card': {
      const opts = {
        enabled: interaction.options.getBoolean('enabled'),
        background: interaction.options.getString('background'),
        backgroundUrl: interaction.options.getString('background_url'),
        textColor: interaction.options.getString('text_color'),
        accentColor: interaction.options.getString('accent_color'),
        overlay: interaction.options.getNumber('overlay'),
        reset: interaction.options.getBoolean('reset') === true,
      };
      const given = opts.reset || Object.entries(opts).some(([k, v]) => k !== 'reset' && v !== null);
      if (!given) {
        const embed = infoEmbed('Welcome Card', cardSummary(s));
        return interaction.editReply(await withCardPreview(embed, interaction.member, s));
      }
      const after = await updateCardStyle(guildId, opts);
      const lines = [cardSummary(after)];
      const warning = cardChannelWarning(guild, after);
      if (warning) lines.push('', warning);
      return interaction.editReply(await withCardPreview(successEmbed('Welcome Card Updated', lines.join('\n')), interaction.member, after));
    }

    case 'dm enable': {
      const after = await saveSettings(guildId, { welcomeDmEnabled: true });
      const lines = ['New members will get a DM. Members with closed DMs are skipped silently.'];
      if (!after.welcomeEnabled) lines.push('⚠️ The Welcome & Goodbye module is off (`/welcome enable`).');
      const { content, embed } = resolveTemplate('dm', after);
      if (!content && !embed && !after.welcomeDmCard) lines.push('⚠️ Nothing would be sent: the DM has no text, embed or card.');
      return reply(successEmbed('Welcome DM Enabled', lines.join('\n')));
    }

    case 'dm disable':
      await saveSettings(guildId, { welcomeDmEnabled: false });
      return reply(successEmbed('Welcome DM Disabled', 'New members will no longer get a welcome DM.'));

    case 'dm card': {
      const enabled = interaction.options.getBoolean('enabled', true);
      await saveSettings(guildId, { welcomeDmCard: enabled });
      return reply(successEmbed(`Welcome DM Card ${enabled ? 'On' : 'Off'}`,
        enabled ? 'The welcome card is attached to the DM.' : 'The welcome DM is sent without the card.'));
    }

    case 'autorole add': {
      const picked = interaction.options.getRole('role', true);
      const role = guild.roles.cache.get(picked.id) ?? picked;
      const delay = interaction.options.getString('delay');
      const { settings: after, already } = await addAutorole({ guild, executor: interaction.member, role, delay });
      const lines = [`<@&${role.id}> is given to new members ${delayText(after.autoroleDelay)}.`];
      if (already) lines.unshift('Already an autorole; the delay was updated.');
      lines.push(...autoroleNotes(guild, after));
      return reply(successEmbed(already ? 'Autorole Delay Updated' : 'Autorole Added', lines.join('\n')));
    }

    case 'autorole remove': {
      const { id } = await removeAutorole(guild, s, interaction.options.getString('role', true));
      if (!id) return reply(errorEmbed('Not an Autorole', 'That role is not in the autorole list. Use `/welcome autorole list`.'));
      const role = guild.roles.cache.get(id);
      return reply(successEmbed('Autorole Removed', role
        ? `<@&${id}> is no longer given to new members.`
        : `Removed the deleted role \`${id}\` from the list.`));
    }

    case 'autorole delay': {
      const { seconds } = await setAutoroleDelay(guildId, interaction.options.getString('duration', true));
      return reply(successEmbed('Autorole Delay Set',
        `Autoroles are given ${delayText(seconds)}. Members already waiting keep the schedule they got when they joined.`));
    }

    case 'autorole list': {
      const { missingIds } = resolveAutoroles(guild, s);
      let current = s;
      let pruned = 0;
      if (missingIds.length && guild.available) {
        pruned = await pruneMissingAutoroles(guild, missingIds);
        current = { ...s, autoroleIds: s.autoroleIds.filter((id) => guild.roles.cache.has(id)) };
      }
      if (!current.autoroleIds.length) {
        return reply(infoEmbed('No Autoroles', 'Add one with `/welcome autorole add`.'));
      }
      const lines = autoroleListLines(guild, current, { prunedCount: pruned });
      return reply(applyGuildColor(infoEmbed('Autoroles', joinWithLimit(lines, { sep: '\n', max: 4000 })), current));
    }

    default:
      return reply(errorEmbed('Unknown Subcommand', 'That subcommand is not available.'));
  }
}
