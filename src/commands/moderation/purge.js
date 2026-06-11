import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';
import { parseDuration, formatDuration } from '../../utils/parseDuration.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

// Discord hard limits
const BULK_DELETE_MAX = 100;
const AGE_LIMIT_MS    = 14 * 24 * 60 * 60 * 1_000; // 14 days

export const data = new SlashCommandBuilder()
  .setName('purge')
  .setDescription('Bulk-delete messages from this channel.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageMessages)
  .addStringOption((o) =>
    o.setName('timeframe')
      .setDescription('Delete messages from the last X time (e.g. 1h, 30m, 2d). Max 14 days.')
  )
  .addIntegerOption((o) =>
    o.setName('amount')
      .setDescription('Max number of messages to delete (default 100). Required if no timeframe.')
      .setMinValue(1)
      .setMaxValue(1000)
  )
  .addUserOption((o) => o.setName('user').setDescription('Only delete messages from this user'))
  .addStringOption((o) =>
    o.setName('filter')
      .setDescription('Only delete messages matching this filter')
      .addChoices(
        { name: 'Bots only',      value: 'bots'        },
        { name: 'Humans only',    value: 'humans'      },
        { name: 'Has embed',      value: 'embeds'      },
        { name: 'Has attachment', value: 'attachments' },
      )
  )
  .addStringOption((o) => o.setName('contains').setDescription('Only delete messages containing this text'));

/**
 * Fetches all messages in a channel newer than cutoffMs, paginating as needed.
 * Stops early once messages older than the cutoff are reached.
 */
async function fetchInTimeframe(channel, cutoffMs) {
  const collected = [];
  let before = undefined;

  while (true) {
    const batch = await channel.messages.fetch({ limit: BULK_DELETE_MAX, before });
    if (batch.size === 0) break;

    let hitCutoff = false;
    for (const msg of batch.values()) {
      if (msg.createdTimestamp <= cutoffMs) { hitCutoff = true; break; }
      collected.push(msg);
    }

    if (hitCutoff || batch.size < BULK_DELETE_MAX) break;
    before = batch.last().id;
  }

  return collected;
}

/** Deletes messages in chunks of 100 via bulkDelete, returns total deleted count. */
async function bulkDeleteAll(channel, messages) {
  let deleted = 0;
  for (let i = 0; i < messages.length; i += BULK_DELETE_MAX) {
    const chunk = messages.slice(i, i + BULK_DELETE_MAX);
    if (chunk.length === 1) {
      // bulkDelete requires ≥2 messages; fall back to single delete
      await chunk[0].delete().catch(() => null);
      deleted += 1;
    } else {
      const result = await channel.bulkDelete(chunk, true);
      deleted += result.size;
    }
  }
  return deleted;
}

function applyFilters(messages, { user, filter, contains }) {
  let out = messages;
  if (user)     out = out.filter((m) => m.author.id === user.id);
  if (contains) out = out.filter((m) => m.content.toLowerCase().includes(contains));
  if (filter === 'bots')        out = out.filter((m) => m.author.bot);
  if (filter === 'humans')      out = out.filter((m) => !m.author.bot);
  if (filter === 'embeds')      out = out.filter((m) => m.embeds.length > 0 || m.attachments.size > 0);
  if (filter === 'attachments') out = out.filter((m) => m.attachments.size > 0);
  return out;
}

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const timeframeStr = interaction.options.getString('timeframe');
  const amount       = interaction.options.getInteger('amount');
  const user         = interaction.options.getUser('user');
  const filter       = interaction.options.getString('filter');
  const contains     = interaction.options.getString('contains')?.toLowerCase();
  const channel      = interaction.channel;

  if (!timeframeStr && !amount) {
    return interaction.editReply({
      embeds: [errorEmbed('Missing Options', 'Provide a `timeframe`, an `amount`, or both.')],
    });
  }

  // Resolve timeframe
  let timeframeMs = null;
  if (timeframeStr) {
    timeframeMs = parseDuration(timeframeStr);
    if (!timeframeMs) {
      return interaction.editReply({
        embeds: [errorEmbed('Invalid Timeframe', `Could not parse \`${timeframeStr}\`. Try \`1h\`, \`30m\`, \`2d\`.`)],
      });
    }
    if (timeframeMs > AGE_LIMIT_MS) {
      return interaction.editReply({
        embeds: [errorEmbed('Timeframe Too Large', 'Discord only allows bulk-deleting messages up to **14 days** old.')],
      });
    }
  }

  // The oldest a message can be and still be bulk-deleted
  const ageCutoff = Date.now() - AGE_LIMIT_MS;
  // The earliest timestamp we care about (whichever is newer: age limit or timeframe)
  const cutoff = timeframeMs ? Math.max(ageCutoff, Date.now() - timeframeMs) : ageCutoff;

  let candidates;
  if (timeframeStr) {
    // Paginate to collect everything in the window
    candidates = await fetchInTimeframe(channel, cutoff);
  } else {
    // Classic mode: fetch exactly `amount` recent messages
    const fetched = await channel.messages.fetch({ limit: Math.min(amount, BULK_DELETE_MAX) });
    candidates = [...fetched.values()].filter((m) => m.createdTimestamp > cutoff);
  }

  // Apply optional filters and cap at `amount` if specified
  let toDelete = applyFilters(candidates, { user, filter, contains });
  if (amount) toDelete = toDelete.slice(0, amount);

  if (toDelete.length === 0) {
    return interaction.editReply({
      embeds: [errorEmbed('Nothing to Delete', 'No messages matched your filters, or all matched messages are older than 14 days.')],
    });
  }

  let deletedCount;
  try {
    deletedCount = await bulkDeleteAll(channel, toDelete);
  } catch (err) {
    return interaction.editReply({ embeds: [errorEmbed('Purge Failed', err.message)] });
  }

  const lines = [`**Deleted:** ${deletedCount} message(s)`];
  if (timeframeStr) lines.push(`**Timeframe:** last ${formatDuration(timeframeMs)}`);
  if (amount)       lines.push(`**Cap:** ${amount}`);
  if (user)         lines.push(`**User:** <@${user.id}>`);
  if (filter)       lines.push(`**Filter:** ${filter}`);
  if (contains)     lines.push(`**Contains:** \`${contains}\``);

  await interaction.editReply({ embeds: [successEmbed('Purge Complete', lines.join('\n'))] });
  setTimeout(() => interaction.deleteReply().catch(() => null), 6_000);
}
