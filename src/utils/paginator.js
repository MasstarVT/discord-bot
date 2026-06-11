import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  ComponentType,
} from 'discord.js';
import { errorEmbed } from './embedBuilder.js';

const PREV_ID = 'paginator:prev';
const NEXT_ID = 'paginator:next';
const STOP_ID = 'paginator:stop';

/**
 * Sends a paginated embed to a channel or replies to an interaction.
 *
 * @param {import('discord.js').ChatInputCommandInteraction | import('discord.js').Message} target
 * @param {import('discord.js').EmbedBuilder[]} pages - Array of embeds (one per page)
 * @param {object} [opts]
 * @param {number}  [opts.timeout=60000]   - Collector idle timeout in ms
 * @param {boolean} [opts.ephemeral=false] - Only relevant for interactions
 */
export async function paginate(target, pages, opts = {}) {
  if (!pages || pages.length === 0) return;

  const { timeout = 60_000, ephemeral = false, startPage = 0 } = opts;

  let page = Math.min(Math.max(startPage, 0), pages.length - 1);

  const footer = (i) => ({ text: `Page ${i + 1} of ${pages.length}` });

  const buildRow = (i) =>
    new ActionRowBuilder().addComponents(
      new ButtonBuilder()
        .setCustomId(PREV_ID)
        .setEmoji('⬅️')
        .setStyle(ButtonStyle.Secondary)
        .setDisabled(i === 0),
      new ButtonBuilder()
        .setCustomId(STOP_ID)
        .setEmoji('⏹️')
        .setStyle(ButtonStyle.Danger),
      new ButtonBuilder()
        .setCustomId(NEXT_ID)
        .setEmoji('➡️')
        .setStyle(ButtonStyle.Secondary)
        .setDisabled(i === pages.length - 1),
    );

  const payload = () => ({
    embeds:     [pages[page].setFooter(footer(page))],
    components: [buildRow(page)],
    ...(ephemeral ? { ephemeral: true } : {}),
  });

  // Support both interactions (fresh or deferred) and plain messages
  let message;
  if (typeof target.reply === 'function' && target.isCommand?.()) {
    if (target.deferred || target.replied) {
      message = await target.editReply({ ...payload() });
    } else {
      message = await target.reply({ ...payload(), fetchReply: true });
    }
  } else {
    message = await target.reply(payload());
  }

  const filter = (i) => {
    const valid = [PREV_ID, NEXT_ID, STOP_ID].includes(i.customId);
    // Only the original invoker can navigate
    const invoker = target.user ?? target.author;
    return valid && i.user.id === invoker?.id;
  };

  const collector = message.createMessageComponentCollector({
    componentType: ComponentType.Button,
    idle: timeout,
    filter,
  });

  collector.on('collect', async (i) => {
    if (i.customId === PREV_ID) page = Math.max(0, page - 1);
    if (i.customId === NEXT_ID) page = Math.min(pages.length - 1, page + 1);
    if (i.customId === STOP_ID) { collector.stop('user'); return; }

    await i.update(payload());
  });

  collector.on('end', async (_, reason) => {
    // Disable all buttons when the collector ends
    const disabledRow = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId(PREV_ID).setEmoji('⬅️').setStyle(ButtonStyle.Secondary).setDisabled(true),
      new ButtonBuilder().setCustomId(STOP_ID).setEmoji('⏹️').setStyle(ButtonStyle.Danger).setDisabled(true),
      new ButtonBuilder().setCustomId(NEXT_ID).setEmoji('➡️').setStyle(ButtonStyle.Secondary).setDisabled(true),
    );
    await message.edit({ components: [disabledRow] }).catch(() => null);
  });
}
