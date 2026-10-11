import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  ComponentType,
  EmbedBuilder,
} from 'discord.js';
import logger from './logger.js';
import { clampText } from './text.js';

const PREV_ID = 'paginator:prev';
const NEXT_ID = 'paginator:next';
const STOP_ID = 'paginator:stop';
const PAGE_INDICATOR_RE = /\bpage\s+\d+\s*(?:\/|of)\s*\d+/i;
const END_REASONS_NO_EDIT = new Set(['messageDelete', 'channelDelete', 'guildDelete', 'threadDelete']);
const TOKEN_FRESH_MS = 14 * 60_000; // interaction tokens live 15 minutes
const FOOTER_MAX = 2048;

/**
 * Sends a paginated embed to an interaction or a message and handles the Prev / Stop / Next
 * buttons. Only the invoker can page; other users are told so and cannot keep it alive.
 * Every callback catches; this throws only when the first page cannot be sent.
 *
 * The caller's embeds are never mutated: pages are snapshotted once and rendered from the snapshot.
 *
 * @param {import('discord.js').RepliableInteraction | import('discord.js').Message} target
 * @param {import('discord.js').EmbedBuilder[]} pages - one embed per page
 * @param {object} [opts]
 * @param {number}  [opts.timeout=60000]   - collector idle timeout in ms (5 s to 14 min)
 * @param {boolean} [opts.ephemeral=false] - only for an interaction that is not yet acknowledged
 *   (a deferred interaction keeps the visibility it was deferred with)
 * @param {number}  [opts.startPage=0]
 * @param {boolean} [opts.followUp=false]  - send with followUp instead of editing the original reply
 *   (use after `deleteReply()`); also stops a component click from overwriting its own panel
 * @param {'append'|'replace'|'none'} [opts.footer='append'] - 'append' adds " • Page X/N" to an
 *   existing footer (or sets one); a footer that already shows a page indicator, and a single page,
 *   are left untouched; 'replace' sets "Page X of N"; 'none' leaves footers alone
 * @returns {Promise<import('discord.js').Message|null>} the sent message, or null when there are no pages
 */
export async function paginate(target, pages, opts = {}) {
  const list = Array.isArray(pages) ? pages.filter(Boolean) : [];
  if (list.length === 0) return null;
  const n = list.length;

  const { ephemeral = false, followUp = false, footer: footerMode = 'append' } = opts;
  const timeout = Math.min(840_000, Math.max(5_000, Math.floor(Number(opts.timeout)) || 60_000));
  let page = Math.min(Math.max(Math.floor(Number(opts.startPage)) || 0, 0), n - 1);

  // Snapshot once, before any render.
  const base = list.map((p) => (typeof p.toJSON === 'function' ? p.toJSON() : { ...(p.data ?? p) }));

  const footerFor = (i) => {
    const original = base[i].footer;
    if (n === 1 || footerMode === 'none') return original;
    if (footerMode === 'replace') return { text: `Page ${i + 1} of ${n}` };
    if (!original?.text) return { text: `Page ${i + 1} of ${n}` };
    if (PAGE_INDICATOR_RE.test(original.text)) return original;
    const suffix = ` • Page ${i + 1}/${n}`;
    return {
      text: clampText(original.text, FOOTER_MAX - suffix.length) + suffix,
      ...(original.icon_url ? { icon_url: original.icon_url } : {}),
    };
  };

  const render = (i) => {
    const footer = footerFor(i);
    return new EmbedBuilder({ ...base[i], ...(footer ? { footer } : {}) });
  };

  const row = (i, disabledAll = false) => new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(PREV_ID).setEmoji('⬅️').setStyle(ButtonStyle.Secondary).setDisabled(i === 0 || disabledAll),
    new ButtonBuilder().setCustomId(STOP_ID).setEmoji('⏹️').setStyle(ButtonStyle.Danger).setDisabled(disabledAll),
    new ButtonBuilder().setCustomId(NEXT_ID).setEmoji('➡️').setStyle(ButtonStyle.Secondary).setDisabled(i === n - 1 || disabledAll),
  );

  const embeds = [render(page)];
  const components = n > 1 ? [row(page)] : [];

  // ── Send ───────────────────────────────────────────────────────────────────
  let message;
  let mode; // 'original' | 'followUp' | 'message'
  if (typeof target.isRepliable === 'function') {
    if (followUp && (target.deferred || target.replied)) {
      message = await target.followUp({ embeds, components, ephemeral });
      mode = 'followUp';
    } else if (target.deferred || target.replied) {
      message = await target.editReply({ embeds, components }); // visibility comes from the defer
      mode = 'original';
    } else {
      const res = await target.reply({ embeds, components, ephemeral, withResponse: true });
      message = res?.resource?.message ?? await target.fetchReply();
      mode = 'original';
    }
  } else {
    message = await target.reply({ embeds, components });
    mode = 'message';
  }

  if (n === 1) return message;

  // ── Collector ──────────────────────────────────────────────────────────────
  const invokerId = target.user?.id ?? target.author?.id;
  const collector = message.createMessageComponentCollector({
    componentType: ComponentType.Button,
    idle: timeout,
    filter: (i) => i.customId.startsWith('paginator:') && i.user.id === invokerId,
  });
  let lastClick = null;
  let stoppedByUser = false;

  // Strangers: 'ignore' fires when the filter rejects a click and, unlike 'collect', does not
  // reset the idle timer, so they can neither page nor keep the paginator alive.
  collector.on('ignore', (i) => {
    try {
      if (!i.customId?.startsWith('paginator:')) return;
      i.reply({ content: "These buttons aren't for you.", ephemeral: true }).catch(() => {});
    } catch {
      // ignore
    }
  });

  // Every click is acknowledged immediately.
  collector.on('collect', async (i) => {
    try {
      lastClick = i;
      if (i.customId === STOP_ID) {
        stoppedByUser = true;
        await i.update({ embeds: [render(page)], components: [row(page, true)] }); // ack + disable in one call
        collector.stop('user');
        return;
      }
      if (i.customId === PREV_ID) page = Math.max(0, page - 1);
      if (i.customId === NEXT_ID) page = Math.min(n - 1, page + 1);
      await i.update({ embeds: [render(page)], components: [row(page)] });
    } catch (err) {
      logger.debug(`paginator collect failed: ${err?.message}`);
    }
  });

  // One edit when it idles out, through the interaction webhook whenever possible: ephemeral
  // messages cannot be edited through the channel route, and @original may have been deleted.
  collector.on('end', async (_collected, reason) => {
    try {
      if (stoppedByUser || END_REASONS_NO_EDIT.has(reason)) return;
      const edit = { components: [row(page, true)] };
      const fresh = (x) => x && Date.now() - x.createdTimestamp < TOKEN_FRESH_MS;
      if (fresh(lastClick)) await lastClick.editReply(edit); // @original of a component interaction is this message
      else if (mode !== 'message' && fresh(target)) await target.editReply({ ...edit, message: message.id });
      else await message.edit(edit);
    } catch (err) {
      logger.debug(`paginator end edit failed: ${err?.message}`);
    }
  });

  return message;
}
