// Descriptor of the `translateStats` collection (Auto-Translate module): translation activity per channel, from the
// TranslateStat counters (src/services/translateStatsService.js), busiest channel first. Each item shows how many messages
// were translated (auto-translate replies and /translate), the languages their authors wrote in and the languages they
// were translated into. Channels set up for auto-translate are listed even before their first translation. Read-only.
// Pure.
import { TRANSLATE_LANGUAGES, describeLanguages } from './translateChannels.js';
import { fmt, toIso } from './levels.js';

/** Languages listed per line before "and N more". */
const SHOWN_LANGUAGES = 8;
const INT_MAX = 2_147_483_647;

const languageLabel = (code) => (code === 'und' ? 'Unknown' : TRANSLATE_LANGUAGES[code] ?? code);

/** "Spanish 62% (80) · English 30% (38)" for a Map of code -> count, busiest first. */
function languageLine(counts) {
  const total = [...counts.values()].reduce((a, b) => a + b, 0);
  if (total === 0) return 'None yet';
  const sorted = [...counts].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  const parts = sorted.slice(0, SHOWN_LANGUAGES)
    .map(([code, n]) => `${languageLabel(code)} ${Math.round((n / total) * 100)}% (${fmt(n)})`);
  if (sorted.length > SHOWN_LANGUAGES) parts.push(`and ${sorted.length - SHOWN_LANGUAGES} more`);
  return parts.join(' · ');
}

const bump = (map, key, n) => map.set(key, (map.get(key) ?? 0) + n);
const later = (a, b) => (!a ? b : !b ? a : (b > a ? b : a));
const earlier = (a, b) => (!a ? b : !b ? a : (b < a ? b : a));

/**
 * TranslateStat rows and the server's auto-translate channels as list items, busiest first.
 * @param {Array<{ channelId: string, via: string, direction: string, lang: string, count: number, firstAt: Date, lastAt: Date }>} rows
 * @param {Array<{ channelId: string, targetLangs: string[] }>} configured
 */
export function buildStatItems(rows, configured = []) {
  const byChannel = new Map();
  const entry = (channelId) => {
    if (!byChannel.has(channelId)) {
      byChannel.set(channelId, { channelId, from: new Map(), to: new Map(), auto: 0, command: 0, firstAt: null, lastAt: null, targets: null });
    }
    return byChannel.get(channelId);
  };
  for (const c of configured) entry(c.channelId).targets = Array.isArray(c.targetLangs) ? c.targetLangs : [];
  for (const r of rows) {
    const e = entry(r.channelId);
    const n = Number(r.count) || 0;
    if (r.direction === 'from') {
      bump(e.from, r.lang, n);
      if (r.via === 'auto') e.auto += n;
      else if (r.via === 'command') e.command += n;
    } else if (r.direction === 'to') {
      bump(e.to, r.lang, n);
    }
    e.firstAt = earlier(e.firstAt, r.firstAt);
    e.lastAt = later(e.lastAt, r.lastAt);
  }
  return [...byChannel.values()]
    .map((e) => {
      const total = e.auto + e.command;
      const top = [...e.from].filter(([code]) => code !== 'und').sort((a, b) => b[1] - a[1]).slice(0, 3)
        .map(([code]) => languageLabel(code));
      const summary = total === 0
        ? 'No translations yet'
        : [`${fmt(total)} message${total === 1 ? '' : 's'}`, top.length ? `mostly ${top.join(', ')}` : null].filter(Boolean).join(' · ');
      return {
        id: e.channelId,
        channelId: e.channelId,
        summary,
        wroteIn: languageLine(e.from),
        translatedInto: languageLine(e.to),
        autoMessages: Math.min(e.auto, INT_MAX),
        commandUses: Math.min(e.command, INT_MAX),
        autoTranslate: e.targets === null ? 'Off' : e.targets.length ? describeLanguages(e.targets) : 'Off',
        firstAt: toIso(e.firstAt),
        lastAt: toIso(e.lastAt),
        total,
      };
    })
    .sort((a, b) => b.total - a.total || (b.lastAt ?? '').localeCompare(a.lastAt ?? '') || a.channelId.localeCompare(b.channelId))
    .map(({ total, ...item }) => item);
}

const readOnly = Object.freeze([
  Object.freeze({
    field: 'channelId', type: 'channel', label: 'Channel', default: null,
    channelTypes: Object.freeze(['text', 'announcement', 'voice', 'stage', 'forum', 'media']),
  }),
  Object.freeze({ field: 'summary', type: 'text', label: 'Activity', maxLength: 200, default: '' }),
  Object.freeze({
    field: 'wroteIn', type: 'text', label: 'Members write in', maxLength: 1000, default: '',
    description: 'The language of each translated message, as the translation service detected it.',
  }),
  Object.freeze({ field: 'translatedInto', type: 'text', label: 'Translated into', maxLength: 1000, default: '' }),
  Object.freeze({ field: 'autoMessages', type: 'int', label: 'Auto-translated messages', min: 0, max: INT_MAX, default: 0 }),
  Object.freeze({ field: 'commandUses', type: 'int', label: '/translate uses', min: 0, max: INT_MAX, default: 0 }),
  Object.freeze({ field: 'autoTranslate', type: 'text', label: 'Auto-translate set to', maxLength: 200, default: '' }),
  Object.freeze({ field: 'firstAt', type: 'datetime', label: 'Counting since', nullable: true, default: null }),
  Object.freeze({ field: 'lastAt', type: 'datetime', label: 'Last translation', nullable: true, default: null }),
]);

export const collection = Object.freeze({
  key: 'translateStats',
  module: 'translation',
  label: 'Channels by translation activity', // also the count chip: "4 channels by translation activity"
  singular: 'channel',
  description: 'Per channel: how many messages were translated (auto-translate replies and /translate), the languages '
    + 'members write in and the languages they were translated into. Only counts are kept, never the messages.',
  limit: null,
  titleField: 'channelId',
  subtitleField: 'summary',
  canCreate: false,
  canEdit: false,
  canDelete: false,
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly,
  actions: Object.freeze([]),
});
