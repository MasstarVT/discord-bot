// Records every call the JS test suite makes to the pure functions of src/utils that bot-util ports, with the value
// each call returned, so the Rust tests can replay exactly the cases the JS tests use.
// Used by bot-util-record.mjs as `node --import <this file> --test ...`; each test process appends its records to
// $BOT_UTIL_RECORD_DIR/<pid>.json when it exits.
import { registerHooks } from 'node:module';
import { writeFileSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT_DIR = process.env.BOT_UTIL_RECORD_DIR;

// module → functions to record. `now` lists functions whose clock default is made explicit before the call (argument
// index and, for an options object, the key), so the record replays deterministically.
const TARGETS = {
  text: ['clampText', 'chunkLines', 'joinWithLimit', 'formatNumber', 'ordinal', 'compactNumber', 'progressBar', 'sanitizeName', 'percentages'],
  parseDuration: ['parseDuration', 'formatDuration', 'formatDurationLong'],
  imageSize: ['readImageSize', 'assertImageDimensions'],
  calendar: ['fitEncoded', 'googleCalendarUrl', 'outlookCalendarUrl', 'icsEscape', 'icsFold', 'buildIcs'],
  time: ['isValidTimeZone', 'zonedParts', 'zonedToUtc', 'localDateKey', 'daysInMonth', 'parseTimeOfDay', 'formatTimeOfDay',
    'parseWeekdays', 'nextTimeOfDay', 'nextIntervalRun', 'parseDateInput', 'parseBirthday', 'discordTimestamp',
    'zoneFromAbbreviation', 'zoneFromOffset', 'parseWhen'],
  timezones: ['listTimeZones', 'normalizeTimeZone', 'utcOffsetLabel', 'searchTimeZones', 'describeLocalTime', 'parseZoneInput',
    'zoneFromRoleName', 'normalizeTimezoneRoles', 'formatInZone', 'zoneLabel', 'zoneSourceText', 'zoneInputError'],
  timeMentions: ['mayMentionTime', 'extractTimeSpans', 'readTimeSpans'],
  safeRegex: ['validateRegex', 'compileSafeRegex', 'escapeRegex', 'containsWord', 'safeRegexTest', 'safeRegexFirstMatch'],
  safeFetch: ['isBlockedAddress'],
  fatalGateway: ['fatalCodeFromError'],
  restartBrake: ['brakeDelayMs'],
  placeholderParser: ['renderPlaceholdersDetailed', 'renderPlaceholders', 'parsePlaceholders', 'placeholderHelp'],
};

// Makes the clock defaults explicit: [argIndex, optionKey|null].
const NOW = {
  'time.parseDateInput': [2, 'now'], 'time.parseWhen': [2, 'now'], 'time.parseBirthday': [1, 'now'],
  'time.nextTimeOfDay': [0, 'after'], 'time.nextIntervalRun': [0, 'after'],
  'timezones.utcOffsetLabel': [1, null], 'timezones.searchTimeZones': [1, 'now'], 'timezones.describeLocalTime': [1, null],
  'timezones.zoneLabel': [1, null], 'timeMentions.readTimeSpans': [2, 'now'], 'calendar.buildIcs': [1, 'now'],
};

const MAX_PER_FN = 4000;
const records = {};
const seen = new Set();

import { enc, plainCtx } from './bot-util-encode.mjs';

function encodeArgs(mod, fn, args) {
  if (mod === 'placeholderParser') {
    if (fn === 'parsePlaceholders') return [enc(args[0]), plainCtx({ member: args[1], guild: args[2] })];
    if (fn !== 'placeholderHelp') return [enc(args[0]), plainCtx(args[1] ?? {}), enc(args[2])];
  }
  return args.map((a) => enc(a));
}

globalThis.__botUtilRecord = function record(mod, fn, original, args) {
  const key = `${mod}.${fn}`;
  const nowSpec = NOW[key];
  let callArgs = [...args];
  if (nowSpec) {
    const [i, opt] = nowSpec;
    while (callArgs.length <= i) callArgs.push(undefined);
    if (opt === null) {
      if (callArgs[i] === undefined) callArgs[i] = new Date();
    } else if (callArgs[i] === undefined || callArgs[i] === null || typeof callArgs[i] === 'object') {
      if (callArgs[i]?.[opt] === undefined) callArgs[i] = { ...(callArgs[i] ?? {}), [opt]: new Date() };
    }
  }
  let quarantinedBefore = false;
  if (mod === 'safeRegex' && (fn === 'safeRegexTest' || fn === 'safeRegexFirstMatch')) {
    const isQ = globalThis.__botUtilIsQuarantined;
    const list = fn === 'safeRegexTest' ? [callArgs[0]] : (Array.isArray(callArgs[0]) ? callArgs[0] : []);
    quarantinedBefore = list.some((r) => r instanceof RegExp && isQ?.(r));
  }
  let encodedArgs = null;
  try { encodedArgs = encodeArgs(mod, fn, callArgs); } catch { encodedArgs = null; }
  const t0 = process.hrtime.bigint();
  let result;
  let threw = null;
  try {
    result = original(...callArgs);
  } catch (e) {
    threw = e;
  }
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  if (encodedArgs && (records[key]?.length ?? 0) < MAX_PER_FN && !(result instanceof Promise)) {
    const slowRegex = mod === 'safeRegex' && (fn === 'safeRegexTest' || fn === 'safeRegexFirstMatch') && (ms > 5 || quarantinedBefore);
    if (!slowRegex) {
      try {
        const out = threw ? { $throw: String(threw?.message) } : enc(result);
        const line = JSON.stringify([encodedArgs, out]);
        const dedupe = `${key}\u0000${JSON.stringify(encodedArgs)}`;
        if (!seen.has(dedupe)) {
          seen.add(dedupe);
          (records[key] ??= []).push([encodedArgs, out]);
        }
        void line;
      } catch {
        // result not serialisable: not recorded
      }
    }
  }
  if (threw) throw threw;
  return result;
};

process.on('exit', () => {
  if (!OUT_DIR || Object.keys(records).length === 0) return;
  writeFileSync(join(OUT_DIR, `${process.pid}.json`), JSON.stringify(records));
});

const UTILS_RE = /\/src\/utils\/([A-Za-z]+)\.js$/;

registerHooks({
  load(url, context, nextLoad) {
    const u = new URL(url);
    if (u.protocol !== 'file:') return nextLoad(url, context);
    const m = UTILS_RE.exec(u.pathname);
    if (!m || !TARGETS[m[1]] || u.searchParams.has('orig')) return nextLoad(url, context);
    const mod = m[1];
    const src = readFileSync(fileURLToPath(u), 'utf8');
    const names = new Set();
    for (const x of src.matchAll(/^export\s+(?:async\s+)?(?:function\*?|const|let|var|class)\s+([A-Za-z_$][\w$]*)/gm)) names.add(x[1]);
    const hasDefault = /^export\s+default\b/m.test(src);
    const origUrl = `${u.href}?orig=1`;
    const lines = [`import * as o from ${JSON.stringify(origUrl)};`];
    if (mod === 'safeRegex') lines.push('globalThis.__botUtilIsQuarantined = o.isQuarantined;');
    for (const n of names) {
      if (TARGETS[mod].includes(n)) {
        lines.push(`export function ${n}(...a) { return globalThis.__botUtilRecord(${JSON.stringify(mod)}, ${JSON.stringify(n)}, o.${n}, a); }`);
      } else {
        lines.push(`export { ${n} } from ${JSON.stringify(origUrl)};`);
      }
    }
    if (hasDefault) lines.push(`export { default } from ${JSON.stringify(origUrl)};`);
    return { format: 'module', source: lines.join('\n'), shortCircuit: true };
  },
});
