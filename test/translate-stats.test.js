// Translation activity: the counters of src/services/translateStatsService.js and the dashboard's `translateStats`
// collection (src/dashboard/collections/translateStats.js and its .store.js). Offline; the RAW_SQL statement runs in
// test/db.test.js.
import { test, describe, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back and no query is ever sent.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const { STAT_VIA, UNKNOWN_LANG, RAW_SQL, recordTranslation, statKeys } = await import('../src/services/translateStatsService.js');
const { buildStatItems, collection } = await import('../src/dashboard/collections/translateStats.js');
const { COLLECTIONS, lintCollections, publicCollection } = await import('../src/dashboard/collections/index.js');
const { STORES } = await import('../src/dashboard/recordsBridge.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

after(async () => {
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const CH = { a: '300000000000000001', b: '300000000000000002', c: '300000000000000003' };
const at = (s) => new Date(`2026-10-${s}T12:00:00.000Z`);
const row = (channelId, via, direction, lang, count, first = '01', last = '02') => ({
  channelId, via, direction, lang, count, firstAt: at(first), lastAt: at(last),
});

describe('statKeys', () => {
  test('one source row, then each target once, sorted', () => {
    assert.deepEqual(statKeys('es', ['ja', 'en', 'ja']), [['from', 'es'], ['to', 'en'], ['to', 'ja']]);
    assert.deepEqual(statKeys('zh-tw', ['en']), [['from', 'zh-tw'], ['to', 'en']]);
  });
  test('an undetected or malformed source counts as unknown; junk targets are dropped', () => {
    assert.deepEqual(statKeys(null, []), [['from', UNKNOWN_LANG]]);
    assert.deepEqual(statKeys('Spanish; DROP', ['EN', 42, null, 'fr']), [['from', UNKNOWN_LANG], ['to', 'fr']]);
    assert.deepEqual(statKeys('es', undefined), [['from', 'es']]);
  });
});

describe('recordTranslation', () => {
  test('ignores DMs, bad ids and unknown sources without touching the database, and never rejects', async () => {
    const before = prisma.$executeRaw;
    let calls = 0;
    prisma.$executeRaw = async () => { calls++; return 0; };
    try {
      await recordTranslation({ guildId: null, channelId: CH.a, via: STAT_VIA.COMMAND, detected: 'es', targets: ['en'] });
      await recordTranslation({ guildId: 'g1', channelId: CH.a, via: STAT_VIA.AUTO, detected: 'es', targets: ['en'] });
      await recordTranslation({ guildId: CH.b, channelId: CH.a, via: 'other', detected: 'es', targets: ['en'] });
    } finally {
      prisma.$executeRaw = before;
    }
    assert.equal(calls, 0);
    // With no database behind it the write fails, and the failure stays inside.
    await assert.doesNotReject(recordTranslation({ guildId: CH.b, channelId: CH.a, via: STAT_VIA.AUTO, detected: 'es', targets: ['en'] }));
  });
  test('exports its upsert for test/db.test.js', () => {
    assert.deepEqual(RAW_SQL.map((e) => e.name), ['translateStats.record']);
    const sql = RAW_SQL[0].build();
    assert.match(sql.sql, /ON CONFLICT \("guildId", "channelId", "via", "direction", "lang"\) DO UPDATE/);
    assert.match(sql.sql, /LEAST\("TranslateStat"\."count"::bigint \+ 1/);
  });
});

describe('buildStatItems', () => {
  const rows = [
    row(CH.a, 'auto', 'from', 'es', 6, '03', '09'),
    row(CH.a, 'auto', 'from', 'en', 3),
    row(CH.a, 'command', 'from', 'und', 1, '01', '05'),
    row(CH.a, 'auto', 'to', 'en', 6),
    row(CH.a, 'auto', 'to', 'es', 3),
    row(CH.a, 'command', 'to', 'ja', 1),
    row(CH.b, 'command', 'from', 'fr', 2, '04', '04'),
    row(CH.b, 'command', 'to', 'en', 2, '04', '04'),
  ];
  const configured = [{ channelId: CH.a, targetLangs: ['en', 'es'] }, { channelId: CH.c, targetLangs: ['de'] }];

  test('one item per channel, busiest first; configured channels without activity come last', () => {
    const items = buildStatItems(rows, configured);
    assert.deepEqual(items.map((i) => i.id), [CH.a, CH.b, CH.c]);
    const [a, b, c] = items;
    assert.equal(a.channelId, CH.a);
    assert.equal(a.autoMessages, 9);
    assert.equal(a.commandUses, 1);
    assert.equal(a.summary, '10 messages · mostly Spanish, English');
    assert.equal(a.wroteIn, 'Spanish 60% (6) · English 30% (3) · Unknown 10% (1)');
    assert.equal(a.translatedInto, 'English 60% (6) · Spanish 30% (3) · Japanese 10% (1)');
    assert.equal(a.autoTranslate, 'English, Spanish');
    assert.equal(a.firstAt, at('01').toISOString());
    assert.equal(a.lastAt, at('09').toISOString());
    assert.equal(b.autoTranslate, 'Off');
    assert.equal(b.summary, '2 messages · mostly French');
    assert.deepEqual(
      { summary: c.summary, wroteIn: c.wroteIn, autoMessages: c.autoMessages, firstAt: c.firstAt, autoTranslate: c.autoTranslate },
      { summary: 'No translations yet', wroteIn: 'None yet', autoMessages: 0, firstAt: null, autoTranslate: 'German' },
    );
  });

  test('long language lists are cut with "and N more"', () => {
    const langs = ['es', 'en', 'fr', 'de', 'it', 'pt', 'nl', 'ja', 'ko', 'ru'];
    const [item] = buildStatItems(langs.map((l, i) => row(CH.a, 'auto', 'from', l, 20 - i)));
    assert.match(item.wroteIn, / · and 2 more$/);
    assert.equal(item.wroteIn.split(' · ').length, 9);
  });
});

describe('translateStats collection', () => {
  test('is registered, read-only, on the Auto-Translate page and lint-clean', () => {
    assert.equal(COLLECTIONS.translateStats, collection);
    assert.ok(STORES.translateStats);
    const pub = publicCollection(collection);
    assert.equal(pub.module, 'translation');
    assert.deepEqual([pub.canCreate, pub.canEdit, pub.canDelete], [false, false, false]);
    assert.deepEqual(lintCollections().filter((p) => p.startsWith('translateStats')), []);
  });

  test('the store refuses every write', async () => {
    const s = STORES.translateStats;
    for (const write of [() => s.create(CH.b, {}), () => s.update(CH.b, CH.a, {}), () => s.remove(CH.b, CH.a)]) {
      await assert.rejects(write(), (err) => err instanceof RecordError && err.code === 'invalid');
    }
  });

  test('the store reads the counters and the channel config of one server', async () => {
    const calls = [];
    const fake = (name, result) => async (args) => { calls.push([name, args.where]); return result; };
    const saved = [prisma.translateStat.findMany, prisma.translateChannel.findMany];
    prisma.translateStat.findMany = fake('stat', rows());
    prisma.translateChannel.findMany = fake('channel', [{ channelId: CH.a, targetLangs: ['en'] }]);
    function rows() { return [row(CH.a, 'auto', 'from', 'es', 2), row(CH.a, 'auto', 'to', 'en', 2)]; }
    try {
      const list = await STORES.translateStats.list(CH.b);
      assert.deepEqual(list.map((i) => [i.id, i.autoMessages, i.autoTranslate]), [[CH.a, 2, 'English']]);
      assert.equal((await STORES.translateStats.get(CH.b, CH.a)).wroteIn, 'Spanish 100% (2)');
    } finally {
      [prisma.translateStat.findMany, prisma.translateChannel.findMany] = saved;
    }
    assert.deepEqual(calls, [
      ['stat', { guildId: CH.b }], ['channel', { guildId: CH.b }],
      ['stat', { guildId: CH.b, channelId: CH.a }], ['channel', { guildId: CH.b, channelId: CH.a }],
    ]);
  });
});
