import {
  SlashCommandBuilder,
  PermissionFlagsBits,
  ModalBuilder,
  TextInputBuilder,
  TextInputStyle,
  ActionRowBuilder,
  EmbedBuilder,
  AttachmentBuilder,
} from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { PLACEHOLDER_LIST } from '../../utils/placeholderParser.js';
import { generateWelcomeCard } from '../../utils/welcomeCard.js';
import { sendWelcome, assignAutorole } from '../../services/welcomeService.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('welcome')
  .setDescription('Configure the welcome & onboarding system.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)

  // ── Enable / Disable ───────────────────────────────────────────────────────
  .addSubcommand((s) => s.setName('enable').setDescription('Enable the welcome module.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Disable the welcome module.'))

  // ── Channel ────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('channel')
      .setDescription('Set the channel where welcome messages are sent.')
      .addChannelOption((o) => o.setName('channel').setDescription('Welcome channel').setRequired(true))
  )

  // ── Embed editor (opens a modal) ───────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('setup')
      .setDescription('Open the embed editor to configure the welcome message.')
  )

  // ── Canvas card toggle ─────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('card')
      .setDescription('Enable or disable the canvas welcome image card.')
      .addBooleanOption((o) => o.setName('enabled').setDescription('Enable image card?').setRequired(true))
  )

  // ── Autorole group ─────────────────────────────────────────────────────────
  .addSubcommandGroup((g) =>
    g.setName('autorole')
      .setDescription('Manage roles auto-assigned when a member joins.')
      .addSubcommand((s) =>
        s.setName('add')
          .setDescription('Add a role to the autorole list.')
          .addRoleOption((o) => o.setName('role').setDescription('Role to assign').setRequired(true))
          .addIntegerOption((o) =>
            o.setName('delay').setDescription('Seconds to wait before assigning (0 = immediate)').setMinValue(0)
          )
      )
      .addSubcommand((s) =>
        s.setName('remove')
          .setDescription('Remove a role from the autorole list.')
          .addRoleOption((o) => o.setName('role').setDescription('Role to remove').setRequired(true))
      )
      .addSubcommand((s) => s.setName('list').setDescription('List all configured autoroles.'))
  )

  // ── Test ───────────────────────────────────────────────────────────────────
  .addSubcommand((s) =>
    s.setName('test').setDescription('Send a test welcome message to this channel.')
  );

// ── Register modal handler at module load ──────────────────────────────────────
// Called by bot.js or any loader — populates client.modalHandlers
export function registerHandlers(client) {
  client.modalHandlers.set('welcome-setup', handleSetupModal);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async function getOrCreateSettings(guildId) {
  return prisma.guildSettings.upsert({
    where:  { guildId },
    update: {},
    create: { guildId },
  });
}

async function updateSettings(guildId, data) {
  const updated = await prisma.guildSettings.update({ where: { guildId }, data });
  await cache.del(`guild:${guildId}:settings`);
  return updated;
}

// ── Modal handler ─────────────────────────────────────────────────────────────

async function handleSetupModal(interaction, client) {
  await interaction.deferReply();

  const title       = interaction.fields.getTextInputValue('welcome-title');
  const description = interaction.fields.getTextInputValue('welcome-description');
  const colorHex    = interaction.fields.getTextInputValue('welcome-color').trim();
  const thumbnail   = interaction.fields.getTextInputValue('welcome-thumbnail').trim().toLowerCase();
  const footer      = interaction.fields.getTextInputValue('welcome-footer');

  const color = parseInt(colorHex.replace('#', ''), 16);
  if (colorHex && isNaN(color)) {
    return interaction.editReply({ embeds: [errorEmbed('Invalid Color', 'Color must be a hex value like `#5865F2`.')] });
  }

  const cfg = {
    title:       title       || null,
    description: description || null,
    color:       colorHex    ? color : COLORS.SUCCESS,
    thumbnail:   ['user_avatar', 'server_icon'].includes(thumbnail) ? thumbnail : 'user_avatar',
    footer:      footer      || null,
    card:        true,
  };

  await updateSettings(interaction.guildId, { welcomeEmbed: cfg });

  await interaction.editReply({
    embeds: [successEmbed(
      'Welcome Embed Saved',
      'Use `/welcome test` to preview the welcome message.'
    )],
  });
}

// ── Main execute ──────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const sub   = interaction.options.getSubcommand(false);
  const group = interaction.options.getSubcommandGroup(false);
  const guild = interaction.guild;

  // Register modal handler if not already (idempotent)
  if (!client.modalHandlers.has('welcome-setup')) {
    client.modalHandlers.set('welcome-setup', handleSetupModal);
  }

  // ── Enable / Disable ───────────────────────────────────────────────────────
  if (sub === 'enable' || sub === 'disable') {
    await interaction.deferReply();
    const enabled = sub === 'enable';
    await updateSettings(guild.id, { welcomeEnabled: enabled });
    return interaction.editReply({
      embeds: [successEmbed(`Welcome Module ${enabled ? 'Enabled' : 'Disabled'}`,
        enabled
          ? 'Set a channel with `/welcome channel` and configure the message with `/welcome setup`.'
          : 'Welcome messages will no longer be sent.')],
    });
  }

  // ── Channel ────────────────────────────────────────────────────────────────
  if (sub === 'channel') {
    await interaction.deferReply();
    const channel = interaction.options.getChannel('channel', true);
    if (!channel.isTextBased()) {
      return interaction.editReply({ embeds: [errorEmbed('Invalid Channel', 'Please select a text channel.')] });
    }
    await updateSettings(guild.id, { welcomeChannelId: channel.id });
    return interaction.editReply({
      embeds: [successEmbed('Welcome Channel Set', `Welcome messages will be sent to <#${channel.id}>.`)],
    });
  }

  // ── Setup modal ────────────────────────────────────────────────────────────
  if (sub === 'setup') {
    const settings = await getOrCreateSettings(guild.id);
    const cfg      = settings.welcomeEmbed ?? {};

    const modal = new ModalBuilder()
      .setCustomId('welcome-setup')
      .setTitle('Welcome Message Setup');

    modal.addComponents(
      new ActionRowBuilder().addComponents(
        new TextInputBuilder()
          .setCustomId('welcome-title')
          .setLabel('Title (leave empty for none)')
          .setStyle(TextInputStyle.Short)
          .setValue(cfg.title ?? '')
          .setRequired(false)
          .setPlaceholder('Welcome {user.username}!')
      ),
      new ActionRowBuilder().addComponents(
        new TextInputBuilder()
          .setCustomId('welcome-description')
          .setLabel('Description / Message body')
          .setStyle(TextInputStyle.Paragraph)
          .setValue(cfg.description ?? '')
          .setRequired(false)
          .setPlaceholder(`Hey {user.mention}, welcome to **{server.name}**!\nYou are member #{server.member_count.ordinal}.`)
          .setMaxLength(2000)
      ),
      new ActionRowBuilder().addComponents(
        new TextInputBuilder()
          .setCustomId('welcome-color')
          .setLabel('Embed color (hex, e.g. #57F287)')
          .setStyle(TextInputStyle.Short)
          .setValue(cfg.color ? `#${cfg.color.toString(16).padStart(6, '0')}` : '#57F287')
          .setRequired(false)
          .setMaxLength(7)
      ),
      new ActionRowBuilder().addComponents(
        new TextInputBuilder()
          .setCustomId('welcome-thumbnail')
          .setLabel('Thumbnail: user_avatar | server_icon | none')
          .setStyle(TextInputStyle.Short)
          .setValue(cfg.thumbnail ?? 'user_avatar')
          .setRequired(false)
      ),
      new ActionRowBuilder().addComponents(
        new TextInputBuilder()
          .setCustomId('welcome-footer')
          .setLabel('Footer text (optional, supports placeholders)')
          .setStyle(TextInputStyle.Short)
          .setValue(cfg.footer ?? '')
          .setRequired(false)
          .setPlaceholder('{server.name} · {server.member_count} members')
      ),
    );

    return interaction.showModal(modal);
  }

  // ── Card toggle ────────────────────────────────────────────────────────────
  if (sub === 'card') {
    await interaction.deferReply();
    const enabled  = interaction.options.getBoolean('enabled', true);
    const settings = await getOrCreateSettings(guild.id);
    const cfg      = { ...(settings.welcomeEmbed ?? {}), card: enabled };
    await updateSettings(guild.id, { welcomeEmbed: cfg });
    return interaction.editReply({
      embeds: [successEmbed(`Welcome Card ${enabled ? 'Enabled' : 'Disabled'}`,
        enabled ? 'A canvas image card will be generated for each new member.' : 'No image card will be sent.')],
    });
  }

  // ── Autorole subcommands ───────────────────────────────────────────────────
  if (group === 'autorole') {
    await interaction.deferReply();
    const settings = await getOrCreateSettings(guild.id);
    const current  = settings.autoroleIds ?? [];

    if (sub === 'add') {
      const role  = interaction.options.getRole('role', true);
      const delay = interaction.options.getInteger('delay') ?? 0;

      if (role.managed) {
        return interaction.editReply({ embeds: [errorEmbed('Managed Role', 'Bot-managed roles cannot be used as autoroles.')] });
      }
      if (role.id === guild.id) {
        return interaction.editReply({ embeds: [errorEmbed('Invalid Role', '@everyone cannot be used as an autorole.')] });
      }
      if (current.includes(role.id)) {
        return interaction.editReply({ embeds: [infoEmbed('Already Added', `<@&${role.id}> is already in the autorole list.`)] });
      }

      await updateSettings(guild.id, {
        autoroleIds:   [...current, role.id],
        autoroleDelay: delay,
      });
      return interaction.editReply({
        embeds: [successEmbed('Autorole Added',
          `<@&${role.id}> will be assigned ${delay > 0 ? `after **${delay}s**` : 'immediately'} when a member joins.`)],
      });
    }

    if (sub === 'remove') {
      const role = interaction.options.getRole('role', true);
      if (!current.includes(role.id)) {
        return interaction.editReply({ embeds: [errorEmbed('Not Found', `<@&${role.id}> is not in the autorole list.`)] });
      }
      await updateSettings(guild.id, { autoroleIds: current.filter((id) => id !== role.id) });
      return interaction.editReply({ embeds: [successEmbed('Autorole Removed', `<@&${role.id}> removed from autorole list.`)] });
    }

    if (sub === 'list') {
      if (current.length === 0) {
        return interaction.editReply({ embeds: [infoEmbed('No Autoroles', 'No autoroles are configured. Add one with `/welcome autorole add`.')] });
      }
      const delay = settings.autoroleDelay ?? 0;
      const list  = current.map((id) => `<@&${id}>`).join('\n');
      return interaction.editReply({
        embeds: [infoEmbed('Autoroles',
          `**Delay:** ${delay > 0 ? `${delay}s after join` : 'Immediate'}\n\n${list}`)],
      });
    }
  }

  // ── Test ───────────────────────────────────────────────────────────────────
  if (sub === 'test') {
    await interaction.deferReply();

    // Temporarily override welcome channel to reply in current channel
    const settings = await getOrCreateSettings(guild.id);
    const cfg      = settings.welcomeEmbed ?? {};

    const embed = new EmbedBuilder()
      .setColor(cfg.color ?? COLORS.SUCCESS)
      .setTitle(cfg.title ? cfg.title.replace(/{[\w.]+}/g, (t) => t) : `👋 Welcome to ${guild.name}!`)
      .setDescription(
        (cfg.description ?? `Hey <@${interaction.user.id}>, welcome to **${guild.name}**!\nYou are our **${guild.memberCount.toLocaleString()}th** member.`)
          .replace('{user.mention}', `<@${interaction.user.id}>`)
          .replace('{user.username}', interaction.user.username)
          .replace('{user.tag}', interaction.user.tag)
          .replace('{server.name}', guild.name)
          .replace('{server.member_count}', guild.memberCount.toLocaleString())
          .replace('{server.member_count.ordinal}', `${guild.memberCount.toLocaleString()}th`)
      )
      .setTimestamp();

    if (cfg.thumbnail === 'server_icon' && guild.iconURL()) {
      embed.setThumbnail(guild.iconURL({ size: 256 }));
    } else {
      embed.setThumbnail(interaction.user.displayAvatarURL({ size: 256 }));
    }

    const payload = { embeds: [embed] };

    if (cfg.card !== false) {
      try {
        const fakeGuild  = { ...guild, memberCount: guild.memberCount };
        const fakeMember = { user: interaction.user, displayAvatarURL: interaction.user.displayAvatarURL.bind(interaction.user) };
        const buffer     = await generateWelcomeCard(fakeMember, fakeGuild);
        const attachment = new AttachmentBuilder(buffer, { name: 'welcome.png' });
        embed.setImage('attachment://welcome.png');
        payload.files = [attachment];
      } catch (err) {
        logger.warn(`Test card generation failed: ${err.message}`);
      }
    }

    return interaction.editReply(payload);
  }
}
