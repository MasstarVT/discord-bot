// Reference data for feat-giveaways-polls, produced by the JS bot's own giveawayService and pollService (no servers).
// Writes rust/fixtures/golden/feat-giveaways-polls/golden.json. Run from the repository root:
//   node rust/fixtures/gen/feat-giveaways-polls-golden.mjs
// Message bodies are what discord.js sends (MessagePayload.resolveBody), embeds and components their toJSON().
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
const quiet = () => {};
console.error = console.warn = console.log = console.debug = console.info = quiet;

const { MessagePayload } = await imp('node_modules', 'discord.js', 'src', 'index.js');
const gs = await imp('src', 'services', 'giveawayService.js');
const ps = await imp('src', 'services', 'pollService.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

const target = { client: { options: { allowedMentions: { parse: ['users'], repliedUser: false } } }, messages: { resolveId: (x) => x } };
const body = (opts) => MessagePayload.create(target, opts).resolveBody().body;
const json = (x) => JSON.parse(JSON.stringify(x));

const NOW = new Date('2026-05-01T12:00:00.000Z');
const SETTINGS = { currencyEmoji: '💎', currencyName: 'gems', embedColor: 0x123456 };
const R1 = '810000000000000001';
const R2 = '810000000000000002';
const R3 = '810000000000000003';
const GONE = '810000000000000099';
const baseG = {
  id: 'cgiveaway0000000000000001',
  guildId: '900000000000000777',
  channelId: '800000000000000001',
  messageId: '700000000000000001',
  hostId: '600000000000000001',
  prize: 'A **shiny** Nitro',
  description: null,
  winnerCount: 2,
  requiredRoleIds: [],
  deniedRoleIds: [],
  roleWeights: [],
  xpPrize: null,
  coinPrize: null,
  pingRoleId: null,
  embedColor: null,
  endsAt: new Date('2026-05-02T12:00:00.000Z'),
  endedAt: null,
  status: 'ACTIVE',
  winnerIds: [],
  createdAt: new Date('2026-05-01T10:00:00.000Z'),
  updatedAt: new Date('2026-05-01T10:00:00.000Z'),
};
const exists = (ids) => (id) => ids.includes(id);

const giveaway = {};
giveaway.joinRows = [
  { id: 'cabc', count: 0, disabled: false },
  { id: 'cabc', count: 1234, disabled: false },
  { id: 'cabc', count: 1, disabled: true },
].map((c) => ({ ...c, row: json(gs.buildJoinRow(c.id, c.count, { disabled: c.disabled }).toJSON()) }));

const embedCases = [
  { name: 'active-plain', g: {}, settings: null, entryCount: 0, roles: [] },
  {
    name: 'active-full',
    g: {
      description: 'Line one\nLine two', requiredRoleIds: [R1, GONE], deniedRoleIds: [R2], xpPrize: 1500, coinPrize: 250, embedColor: 0,
      roleWeights: [{ roleId: R3, weight: 3 }, { roleId: R1, weight: 2 }, { roleId: GONE, weight: 9 }],
    },
    settings: SETTINGS, entryCount: 12, roles: [R1, R2, R3],
  },
  { name: 'active-colour', g: { embedColor: 0xff0000, coinPrize: 5 }, settings: null, entryCount: 3, roles: [] },
  { name: 'draft', g: { status: 'DRAFT', messageId: null }, settings: SETTINGS, entryCount: 4, roles: [] },
  {
    name: 'ended-winners', g: { status: 'ENDED', endedAt: new Date('2026-05-02T11:00:00.000Z'), winnerIds: ['600000000000000002', '600000000000000003'], xpPrize: 10 },
    settings: SETTINGS, entryCount: 1001, roles: [],
  },
  { name: 'ended-none', g: { status: 'ENDED', endedAt: new Date('2026-05-02T11:00:00.000Z'), description: 'desc' }, settings: null, entryCount: 0, roles: [] },
  { name: 'cancelled', g: { status: 'CANCELLED', endedAt: new Date('2026-05-01T13:00:00.000Z') }, settings: SETTINGS, entryCount: 7, roles: [] },
  { name: 'cancelled-no-end', g: { status: 'CANCELLED' }, settings: null, entryCount: 0, roles: [] },
  { name: 'long-prize', g: { prize: 'P'.repeat(300), description: 'D'.repeat(1200) }, settings: null, entryCount: 0, roles: [] },
];
giveaway.embeds = embedCases.map((c) => {
  const g = { ...baseG, ...c.g };
  return {
    name: c.name, g: json(g), settings: c.settings, entryCount: c.entryCount, roles: c.roles, now: NOW.toISOString(),
    embed: json(gs.buildGiveawayEmbed(g, { settings: c.settings, entryCount: c.entryCount, roleExists: exists(c.roles), now: NOW }).toJSON()),
  };
});

giveaway.listLines = [
  { id: 'c1', prize: 'Thing', status: 'ACTIVE', channelId: '800000000000000001', endsAt: baseG.endsAt, endedAt: null, _count: { entries: 1 } },
  { id: 'c2', prize: 'X'.repeat(100), status: 'DRAFT', channelId: '800000000000000001', endsAt: baseG.endsAt, endedAt: null, _count: { entries: 0 } },
  { id: 'c3', prize: 'Thing', status: 'ENDED', channelId: '800000000000000001', endsAt: baseG.endsAt, endedAt: NOW, _count: { entries: 12345 } },
  { id: 'c4', prize: 'Thing', status: 'ENDED', channelId: '800000000000000001', endsAt: baseG.endsAt, endedAt: null, _count: { entries: 2 } },
  { id: 'c5', prize: 'Thing', status: 'CANCELLED', channelId: '800000000000000001', endsAt: baseG.endsAt, endedAt: null, _count: { entries: 2 } },
].map((row) => ({ row: json(row), line: gs.formatListLine(row) }));

const fakeChannel = () => {
  const sent = [];
  return { sent, async send(p) { sent.push(body(p)); return { id: '1' }; } };
};
const winnersOf = (n) => Array.from({ length: n }, (_, i) => `6000000000000${String(10000 + i).padStart(5, '0')}`);
giveaway.announce = [];
for (const [name, g, winners, settings, reroll] of [
  ['none', baseG, [], null, false],
  ['one', { ...baseG, xpPrize: 50, coinPrize: 7 }, winnersOf(1), SETTINGS, false],
  ['many', baseG, winnersOf(90), null, false],
  ['reroll-two', { ...baseG, messageId: null }, winnersOf(2), null, true],
  ['long-prize', { ...baseG, prize: 'Z'.repeat(250), xpPrize: 1 }, winnersOf(60), null, false],
]) {
  const ch = fakeChannel();
  await gs.announceWinners(ch, g, winners, { settings, reroll });
  giveaway.announce.push({ name, g: json(g), winners, settings, reroll, bodies: ch.sent });
}

giveaway.pure = {
  normalize: [
    null, 'x', [], [{ roleId: R1, weight: 2 }, { roleId: R1, weight: 5 }, { roleId: 'bad', weight: 3 }, { roleId: R2, weight: 11 }, { roleId: R3, weight: 2.5 }, { roleId: R2, weight: 2.0 }],
    Array.from({ length: 13 }, (_, i) => ({ roleId: `81000000000000${String(i).padStart(4, '0')}`, weight: (i % 10) + 1 })),
  ].map((input) => ({ input, out: gs.normalizeRoleWeights(input) })),
  pack: [
    { ids: [], max: 1700 }, { ids: winnersOf(3), max: 1700 }, { ids: winnersOf(90), max: 1700 }, { ids: winnersOf(10), max: 50 }, { ids: winnersOf(5), max: 10 },
  ].map((c) => ({ ...c, out: gs.packMentions(c.ids, c.max) })),
  refs: [
    'cl1234567890abcdefghij123', 'cXYZ', '700000000000000001', ' 700000000000000001 ', '12345',
    'https://discord.com/channels/900000000000000777/800000000000000001/700000000000000001',
    'https://ptb.discordapp.com/channels/900000000000000777/800000000000000001/700000000000000001/',
    'http://canary.discord.com/channels/1/2/3', 'https://example.com/channels/900000000000000777/800000000000000001/700000000000000001', '',
  ].map((input) => ({ input, out: gs.parseGiveawayRef(input) })),
  endsAt: [null, -5, 30_000, 60_000, 90 * 86_400_000, 90 * 86_400_000 + 1].map((d) => ({
    delta: d, out: gs.validateEndsAt(d === null ? new Date(NaN) : new Date(NOW.getTime() + d), NOW, 'Europe/Berlin'),
  })),
};

// The lazy draw against a recorded rng sequence and a fixed resolver.
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return (((t ^ (t >>> 14)) >>> 0) + 0.5) / 4294967297;
  };
}
giveaway.draws = [];
for (let seed = 1; seed <= 40; seed++) {
  const rng = mulberry32(seed);
  const n = 5 + (seed * 7) % 60;
  const userIds = Array.from({ length: n }, (_, i) => `u${(i * 13 + seed) % (n + 3)}`); // a few duplicates
  const table = {};
  for (let i = 0; i < n + 3; i++) {
    const r = (i * 31 + seed) % 10;
    table[`u${i}`] = r === 0 ? 'ineligible' : r === 1 ? 'unknown' : [1, 2, 10][r % 3];
  }
  const us = Array.from({ length: 200 }, () => rng());
  let k = 0;
  const count = 1 + (seed % 5);
  const maxWeight = 10;
  const batchSize = 1 + (seed % 9);
  const calls = [];
  const out = await gs.drawWinners({
    userIds, count, maxWeight, batchSize, rng: () => us[k++],
    resolve: async (ids) => {
      calls.push(ids.length);
      const m = new Map();
      for (const id of ids) {
        const v = table[id];
        if (v === 'unknown') continue;
        m.set(id, v === 'ineligible' ? null : { weight: v });
      }
      return m;
    },
  });
  giveaway.draws.push({ seed, userIds, count, maxWeight, batchSize, us: us.slice(0, k), table, out, calls });
}

giveaway.customIds = {
  join: buildCustomId('giveaway', 'join', 'cabc123'),
  endJobKey: gs.endJobKey('cabc123'),
  endJobType: gs.END_JOB_TYPE,
};

// ── Polls ──────────────────────────────────────────────────────────────────
const baseP = {
  id: 'cpoll000000000000000000001',
  guildId: '900000000000000777',
  channelId: '800000000000000001',
  messageId: '700000000000000005',
  authorId: '600000000000000001',
  question: 'What should we play [tonight]?',
  options: ['Chess', 'Go', 'Poker'],
  maxChoices: 1,
  allowedRoleIds: [],
  deniedRoleIds: [],
  hideResults: false,
  endsAt: new Date('2026-05-03T12:00:00.000Z'),
  status: 'ACTIVE',
  createdAt: new Date('2026-05-01T09:00:00.000Z'),
  updatedAt: new Date('2026-05-01T09:00:00.000Z'),
};
const tallies = {
  empty: ps.emptyTally(3),
  some: { counts: [3, 1, 0], votes: 4, voters: 4 },
  tie: { counts: [2, 2, 1], votes: 5, voters: 3 },
  thirds: { counts: [1, 1, 1], votes: 3, voters: 1 },
};
const polls = {};
const pollCases = [
  { name: 'single', p: {} },
  { name: 'multi', p: { maxChoices: 2, options: ['Chess', 'Go', 'Poker', 'Bridge'], allowedRoleIds: [R1], deniedRoleIds: [R2, R3] } },
  { name: 'hidden', p: { hideResults: true, endsAt: null } },
  { name: 'ended', p: { status: 'ENDED' } },
  { name: 'many-long', p: { options: Array.from({ length: 12 }, (_, i) => `Option number ${i + 1} which is long`), messageId: null } },
];
polls.components = [];
polls.embeds = [];
polls.payloads = [];
polls.results = [];
for (const c of pollCases) {
  const p = { ...baseP, ...c.p };
  const n = p.options.length;
  const t = n === 3 ? tallies.some : { counts: Array.from({ length: n }, (_, i) => (i * 3) % 4), votes: 9, voters: 5 };
  for (const [opt, label] of [[{}, 'live'], [{ disabled: true }, 'disabled'], [{ selected: [0, 2] }, 'selected']]) {
    polls.components.push({ name: `${c.name}-${label}`, p: json(p), opts: opt, rows: json(ps.buildPollComponents(p, opt).map((r) => r.toJSON())) });
  }
  for (const ended of [false, true]) {
    polls.embeds.push({ name: `${c.name}-${ended}`, p: json(p), tally: t, settings: SETTINGS, ended, embed: json(ps.buildPollEmbed(p, t, { settings: SETTINGS, ended }).toJSON()) });
  }
  polls.payloads.push({ name: c.name, p: json(p), tally: t, body: body(ps.buildPollPayload(p, t, { settings: null })) });
  for (const [tn, tt] of Object.entries(tallies)) {
    if (n !== 3) continue;
    for (const live of [false, true]) {
      for (const messageExists of [true, false]) {
        polls.results.push({
          name: `${c.name}-${tn}-${live}-${messageExists}`, p: json(p), tally: tt, live, messageExists,
          embed: json(ps.buildResultsEmbed(p, tt, { settings: null, live, messageExists }).toJSON()),
        });
      }
    }
  }
}
polls.modal = json(ps.buildCreateModal('1234567890').toJSON());
polls.listLines = ps.pollListLines(
  [baseP, { ...baseP, id: 'c2', messageId: null, endsAt: null, question: 'Q'.repeat(90) }, { ...baseP, id: 'c3', endsAt: new Date(NOW.getTime() - 1000) }],
  new Map([[baseP.id, 1], ['c3', 2500]]),
  NOW.getTime(),
);
polls.listInput = [json(baseP), json({ ...baseP, id: 'c2', messageId: null, endsAt: null, question: 'Q'.repeat(90) }), json({ ...baseP, id: 'c3', endsAt: new Date(NOW.getTime() - 1000) })];
polls.optionsParse = [
  '', 'one', 'a\nb', '1. Yes\n2) No\n- Maybe\n* Sure\n• Bullet\n10. Ten\n100. Hundred', 'A\r\n  a  \nB\nb\nC',
  Array.from({ length: 22 }, (_, i) => `o${i}`).join('\n'), Array.from({ length: 8 }, (_, i) => `${'x'.repeat(81)}${i}`).join('\n'),
  '-NoSpace\n1.NoSpace\n  \n\n', '🎉'.repeat(41) + '\nok',
].map((raw) => ({ raw, out: ps.parsePollOptions(raw) }));
polls.questions = ['', '   ', 'Why?', '  padded  ', 'Q'.repeat(257), '🎉'.repeat(129)].map((raw) => ({ raw, out: ps.validateQuestion(raw) }));
polls.durations = [null, '', '  ', '30s', '1m', '2h', '30d', '31d', 'nonsense', '1w'].map((raw) => ({ raw, out: ps.parseDurationOption(raw) }));
polls.canVote = [];
for (const held of [[], [R1], [R2], [R1, R2]]) {
  for (const poll of [{}, { allowedRoleIds: [R1] }, { deniedRoleIds: [R2] }, { allowedRoleIds: [GONE] }, { allowedRoleIds: [GONE, R1] }]) {
    for (const existing of [null, [R1, R2]]) {
      polls.canVote.push({ held, poll, existing, out: ps.canVote(held, poll, existing ? new Set(existing) : null) });
    }
  }
}
polls.toggles = [];
for (const cur of [[], [1], [0, 2]]) for (const idx of [0, 1, 2]) for (const max of [1, 2, 3]) polls.toggles.push({ cur, idx, max, out: ps.applyVoteToggle(cur, idx, max) });
polls.selections = [[[], 3, 2], [['0', '2'], 3, 2], [['2', '0', '2'], 3, 2], [['3'], 3, 3], [['a'], 3, 3], [['0', '1', '2'], 3, 2], [['007'], 9, 3], [['01'], 9, 3]]
  .map(([values, n, max]) => ({ values, n, max, out: ps.normalizeSelection(values, n, max) }));
polls.optionLines = [];
for (const [tn, t] of Object.entries(tallies)) for (const hidden of [false, true]) for (const ended of [false, true]) {
  polls.optionLines.push({ tally: tn, hidden, ended, out: ps.formatOptionLines(baseP.options, t, { hidden, ended }) });
}
polls.tallies = tallies;
polls.tallyFromGroups = [
  { n: 3, groups: [{ optionIndex: 0, _count: { _all: 2 } }, { optionIndex: 5, _count: { _all: 9 } }, { optionIndex: 2, _count: { _all: 1 } }], totals: { votes: 1, voters: 2 } },
].map((c) => ({ ...c, out: ps.tallyFromGroups(c.n, c.groups, c.totals) }));
polls.sendFailure = [50013, 50001, 50083, 10003, 40005, null].map((code) => ({ code, out: ps.sendFailureReason(code === null ? {} : { code }) }));
polls.customIds = {
  vote: buildCustomId('poll', 'vote', 'cpoll1', '3'),
  select: buildCustomId('poll-select', 'cpoll1'),
  modal: buildCustomId('poll-modal', '1234567890'),
  endJobKey: ps.endJobKey('cpoll1'),
  endJobType: ps.END_JOB_TYPE,
};
polls.constants = ps.POLL;

const out = { now: NOW.toISOString(), settings: SETTINGS, giveaway, polls };
const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-giveaways-polls');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), `${JSON.stringify(out, null, 1)}\n`);
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.stdout.write('\nRESULT {"ok":true}\n');
process.exit(0);
