// Descriptor of the `translateChannels` collection: channels whose messages the bot translates (/autotranslate).
// Pure: constants only. The language table is a copy of LANGUAGES in translationService.js (the service opens Redis and Prisma
// at import); test/dashboard-collections-more.test.js fails when the two drift apart.
import { LIMITS } from '../../config/constants.js';

/** Languages a channel can be translated into: code -> English name (= LANGUAGES in translationService.js). */
export const TRANSLATE_LANGUAGES = Object.freeze({
  af: 'Afrikaans',
  ar: 'Arabic',
  az: 'Azerbaijani',
  be: 'Belarusian',
  bg: 'Bulgarian',
  bn: 'Bengali',
  bs: 'Bosnian',
  ca: 'Catalan',
  cs: 'Czech',
  cy: 'Welsh',
  da: 'Danish',
  de: 'German',
  el: 'Greek',
  en: 'English',
  eo: 'Esperanto',
  es: 'Spanish',
  et: 'Estonian',
  fa: 'Persian',
  fi: 'Finnish',
  fr: 'French',
  ga: 'Irish',
  gl: 'Galician',
  gu: 'Gujarati',
  he: 'Hebrew',
  hi: 'Hindi',
  hr: 'Croatian',
  hu: 'Hungarian',
  hy: 'Armenian',
  id: 'Indonesian',
  is: 'Icelandic',
  it: 'Italian',
  ja: 'Japanese',
  ka: 'Georgian',
  kk: 'Kazakh',
  km: 'Khmer',
  ko: 'Korean',
  lt: 'Lithuanian',
  lv: 'Latvian',
  mk: 'Macedonian',
  ml: 'Malayalam',
  mn: 'Mongolian',
  mr: 'Marathi',
  ms: 'Malay',
  mt: 'Maltese',
  my: 'Myanmar (Burmese)',
  ne: 'Nepali',
  nl: 'Dutch',
  no: 'Norwegian',
  pa: 'Punjabi',
  pl: 'Polish',
  pt: 'Portuguese',
  ro: 'Romanian',
  ru: 'Russian',
  si: 'Sinhala',
  sk: 'Slovak',
  sl: 'Slovenian',
  sq: 'Albanian',
  sr: 'Serbian',
  sv: 'Swedish',
  sw: 'Swahili',
  ta: 'Tamil',
  te: 'Telugu',
  th: 'Thai',
  tl: 'Filipino',
  tr: 'Turkish',
  uk: 'Ukrainian',
  ur: 'Urdu',
  uz: 'Uzbek',
  vi: 'Vietnamese',
  xh: 'Xhosa',
  yi: 'Yiddish',
  zh: 'Chinese (Simplified)',
  'zh-tw': 'Chinese (Traditional)',
  zu: 'Zulu',
});

/** /autotranslate takes up to five target languages per channel. */
export const MAX_TARGETS = 5;

const fields = Object.freeze([
  Object.freeze({
    field: 'channelId',
    type: 'channel',
    label: 'Channel',
    description: 'Messages posted here are translated by the bot. Text, announcement, voice and stage channels (their chat) work; '
      + 'threads are set up with /autotranslate in Discord.',
    channelTypes: Object.freeze(['text', 'announcement', 'voice', 'stage']),
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'targetLangs',
    type: 'multiEnum',
    label: 'Translate into',
    description: 'Up to five languages. Every message is translated into each of them (the bot posts a reply with the translations).',
    options: Object.freeze(Object.entries(TRANSLATE_LANGUAGES).map(([value, name]) => Object.freeze({ value, label: `${name} (${value})` }))),
    minItems: 1,
    maxItems: MAX_TARGETS,
    default: Object.freeze(['en']),
    required: true,
  }),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'languages', type: 'text', label: 'Languages', maxLength: 200, default: '' }),
]);

/** "Spanish, Japanese" for a list of codes. */
export const describeLanguages = (codes) => codes.map((c) => TRANSLATE_LANGUAGES[c] ?? c).join(', ');

export const collection = Object.freeze({
  key: 'translateChannels',
  module: 'translation',
  label: 'Translated channels',
  singular: 'translated channel',
  description: 'Channels where the bot translates every message. Adding the first channel also turns the Auto-Translate module on, '
    + 'like /autotranslate does. Deleting a channel here only stops the translating. The bot picks up changes within about 30 seconds.',
  limit: LIMITS.TRANSLATE_CHANNELS,
  titleField: 'languages',
  subtitleField: 'channelId',
  fields,
  createOnly: Object.freeze(['channelId']),
  readOnly,
  actions: Object.freeze([]),
});
