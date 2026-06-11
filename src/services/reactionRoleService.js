import {
  EmbedBuilder,
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  StringSelectMenuBuilder,
} from 'discord.js';
import prisma from '../database/client.js';
import logger from '../utils/logger.js';

// ── Panel builder ─────────────────────────────────────────────────────────────

/**
 * Builds the embed + components payload for a reaction role panel.
 * Safe to call with 0 items (shows an "empty" placeholder).
 *
 * @param {import('@prisma/client').ReactionRoleGroup & { items: import('@prisma/client').ReactionRoleItem[] }} group
 * @returns {{ embeds: EmbedBuilder[], components: ActionRowBuilder[] }}
 */
export function buildPanelPayload(group) {
  const embed = new EmbedBuilder()
    .setColor(0x5865f2)
    .setTitle(group.title)
    .setDescription(group.description ?? 'Click below to manage your roles.')
    .setFooter({ text: `${group.type} · ${group.layout}` })
    .setTimestamp();

  const items = [...group.items].sort((a, b) => a.position - b.position);

  if (items.length === 0) {
    embed.setDescription((group.description ?? '') + '\n\n*No roles have been added to this panel yet.*');
    return { embeds: [embed], components: [] };
  }

  if (group.layout === 'SELECT') {
    const isUnique  = group.type === 'UNIQUE';
    const maxValues = isUnique ? 1 : Math.min(items.length, group.maxRoles ?? 25);

    const select = new StringSelectMenuBuilder()
      .setCustomId(`rr-select:${group.id}`)
      .setPlaceholder('Select a role…')
      .setMinValues(0)
      .setMaxValues(maxValues)
      .addOptions(
        items.map((item) => ({
          label:       item.label,
          value:       item.roleId,
          ...(item.emoji ? { emoji: item.emoji } : {}),
        }))
      );

    return { embeds: [embed], components: [new ActionRowBuilder().addComponents(select)] };
  }

  // BUTTONS layout — pack into rows of 5 (max 5 rows = 25 buttons)
  const rows = [];
  for (let i = 0; i < Math.min(items.length, 25); i += 5) {
    const row = new ActionRowBuilder().addComponents(
      items.slice(i, i + 5).map((item) => {
        const btn = new ButtonBuilder()
          .setCustomId(item.customId)
          .setLabel(item.label)
          .setStyle(item.style ?? ButtonStyle.Primary);
        if (item.emoji) btn.setEmoji(item.emoji);
        return btn;
      })
    );
    rows.push(row);
  }

  return { embeds: [embed], components: rows };
}

// ── Shared role-assignment logic ──────────────────────────────────────────────

/**
 * @typedef {Object} AssignResult
 * @property {boolean} success
 * @property {string}  message  Human-readable outcome for ephemeral reply
 */

/**
 * Applies the group's role archetype logic for a given member + role selection.
 *
 * @param {import('discord.js').GuildMember} member
 * @param {import('@prisma/client').ReactionRoleGroup} group
 * @param {import('@prisma/client').ReactionRoleItem[]} items  All items in the group
 * @param {string[]} selectedRoleIds  The role IDs the user selected/clicked
 * @returns {Promise<AssignResult>}
 */
async function applyRoleLogic(member, group, items, selectedRoleIds) {
  const groupRoleIds = items.map((i) => i.roleId);

  try {
    switch (group.type) {

      case 'NORMAL': {
        // Toggle each selected role; deselected roles (not in selectedRoleIds) are removed.
        // For button interactions selectedRoleIds has exactly 1 element.
        const toAdd    = selectedRoleIds.filter((id) => !member.roles.cache.has(id));
        const toRemove = selectedRoleIds.filter((id) =>  member.roles.cache.has(id));

        // Enforce maxRoles cap for adds
        if (group.maxRoles && toAdd.length > 0) {
          const currentGroupRoles = member.roles.cache.filter((r) => groupRoleIds.includes(r.id)).size;
          const slotsLeft = group.maxRoles - currentGroupRoles;
          if (slotsLeft <= 0) {
            return { success: false, message: `You can only hold **${group.maxRoles}** role(s) from this panel. Remove one first.` };
          }
          toAdd.splice(slotsLeft); // trim to available slots
        }

        if (toAdd.length > 0)    await member.roles.add(toAdd,    'Reaction role — NORMAL');
        if (toRemove.length > 0) await member.roles.remove(toRemove, 'Reaction role — NORMAL');

        const addedNames   = toAdd.map((id)    => member.guild.roles.cache.get(id)?.name ?? id);
        const removedNames = toRemove.map((id) => member.guild.roles.cache.get(id)?.name ?? id);
        const parts = [];
        if (addedNames.length)   parts.push(`✅ Added: **${addedNames.join(', ')}**`);
        if (removedNames.length) parts.push(`❌ Removed: **${removedNames.join(', ')}**`);
        return { success: true, message: parts.join('\n') || 'No changes.' };
      }

      case 'UNIQUE': {
        const [targetId] = selectedRoleIds;
        if (!targetId) {
          // User deselected everything — remove all group roles
          const toRemove = groupRoleIds.filter((id) => member.roles.cache.has(id));
          if (toRemove.length > 0) await member.roles.remove(toRemove, 'Reaction role — UNIQUE deselect');
          return { success: true, message: '❌ Role removed.' };
        }
        const alreadyHas = member.roles.cache.has(targetId);
        if (alreadyHas) {
          // Clicking the same role again removes it (toggle behaviour for UNIQUE)
          const currentGroupRoles = groupRoleIds.filter((id) => member.roles.cache.has(id));
          await member.roles.remove(currentGroupRoles, 'Reaction role — UNIQUE toggle off');
          return { success: true, message: `❌ **${member.guild.roles.cache.get(targetId)?.name}** removed.` };
        }
        // Remove current group roles, add the selected one
        const toRemove = groupRoleIds.filter((id) => id !== targetId && member.roles.cache.has(id));
        if (toRemove.length > 0) await member.roles.remove(toRemove, 'Reaction role — UNIQUE swap');
        await member.roles.add(targetId, 'Reaction role — UNIQUE');
        return { success: true, message: `✅ **${member.guild.roles.cache.get(targetId)?.name}** assigned.` };
      }

      case 'VERIFY': {
        const toAdd = selectedRoleIds.filter((id) => !member.roles.cache.has(id));
        if (toAdd.length === 0) {
          return { success: false, message: 'You already have this role.' };
        }
        await member.roles.add(toAdd, 'Reaction role — VERIFY');
        return { success: true, message: `✅ Verified! Role assigned.` };
      }

      case 'REVERSE': {
        const toRemove = selectedRoleIds.filter((id) => member.roles.cache.has(id));
        if (toRemove.length === 0) {
          return { success: false, message: "You don't have this role." };
        }
        await member.roles.remove(toRemove, 'Reaction role — REVERSE');
        const names = toRemove.map((id) => member.guild.roles.cache.get(id)?.name ?? id);
        return { success: true, message: `❌ **${names.join(', ')}** removed.` };
      }

      default:
        return { success: false, message: 'Unknown role logic type.' };
    }
  } catch (err) {
    logger.error(`reactionRoleService: role assignment failed for ${member.user.tag}`, err);
    return { success: false, message: `Failed to update roles: ${err.message}` };
  }
}

// ── Interaction handlers ───────────────────────────────────────────────────────

async function fetchGroupWithItems(groupId) {
  return prisma.reactionRoleGroup.findUnique({
    where:   { id: groupId },
    include: { items: { orderBy: { position: 'asc' } } },
  });
}

async function handleRoleButton(interaction, client) {
  // customId format: "rr:{groupId}:{roleId}"
  const parts    = interaction.customId.split(':');
  const groupId  = parts[1];
  const roleId   = parts[2];

  await interaction.deferReply({ ephemeral: true });

  const group = await fetchGroupWithItems(groupId);
  if (!group || group.guildId !== interaction.guildId) {
    return interaction.editReply({ content: '⚠️ This panel is no longer active.' });
  }

  const member = interaction.member;
  const result = await applyRoleLogic(member, group, group.items, [roleId]);

  await interaction.editReply({ content: result.message });
}

async function handleRoleSelect(interaction, client) {
  // customId format: "rr-select:{groupId}"
  const groupId       = interaction.customId.split(':')[1];
  const selectedRoles = interaction.values; // array of roleId strings

  await interaction.deferReply({ ephemeral: true });

  const group = await fetchGroupWithItems(groupId);
  if (!group || group.guildId !== interaction.guildId) {
    return interaction.editReply({ content: '⚠️ This panel is no longer active.' });
  }

  const member = interaction.member;
  const result = await applyRoleLogic(member, group, group.items, selectedRoles);

  await interaction.editReply({ content: result.message });
}

// ── Panel sync helper (called after add/remove role item) ─────────────────────

/**
 * Re-fetches a group's items and edits the Discord message to reflect changes.
 *
 * @param {import('discord.js').Guild} guild
 * @param {string} groupId
 * @param {import('discord.js').Client} client
 */
export async function syncPanel(guild, groupId, client) {
  const group = await fetchGroupWithItems(groupId);
  if (!group) return;

  const channel = guild.channels.cache.get(group.channelId)
    ?? await guild.channels.fetch(group.channelId).catch(() => null);
  if (!channel?.isTextBased()) return;

  const message = await channel.messages.fetch(group.messageId).catch(() => null);
  if (!message) return;

  const { embeds, components } = buildPanelPayload(group);
  await message.edit({ embeds, components }).catch((err) =>
    logger.error(`syncPanel: failed to edit panel message ${group.messageId}`, err)
  );
}

// ── Handler registration ──────────────────────────────────────────────────────

/**
 * Registers the persistent button and select menu handlers for reaction roles.
 * Must be called once at startup (from bot.js or commandLoader via registerHandlers).
 *
 * @param {import('discord.js').Client} client
 */
export function registerHandlers(client) {
  client.buttonHandlers.set('rr',        handleRoleButton);
  client.selectHandlers.set('rr-select', handleRoleSelect);
}
