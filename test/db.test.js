// Opt-in database test: runs every exported RAW_SQL statement against a MIGRATED Postgres database and checks the
// raw-SQL time helpers. Skipped unless TEST_DATABASE_URL is set, so `npm test` stays offline:
//   TEST_DATABASE_URL='postgresql://...' node --test --test-concurrency=1 test/db.test.js
// (CI runs the whole suite that way after `prisma migrate deploy`.) Everything happens inside one transaction that is
// rolled back at the end, so the database is left exactly as it was found.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, resolve, dirname, relative } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const DB = process.env.TEST_DATABASE_URL;

if (!DB) {
  test('db: raw SQL against a migrated database', { skip: 'TEST_DATABASE_URL not set' }, () => {});
} else {
  // Offline for everything except the database under test. DATABASE_URL must be set before client.js is imported.
  Object.assign(process.env, {
    NODE_ENV: 'test',
    DISCORD_TOKEN: 'test-token',
    CLIENT_ID: '000000000000000000',
    REDIS_URL: process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1',
    DATABASE_URL: DB,
  });

  const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const SRC = join(ROOT, 'src');
  const walk = (dir) => readdirSync(dir).flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') ? [full] : [];
  });
  const rel = (file) => relative(ROOT, file).replace(/\\/g, '/');

  // Nothing listens on the Redis URL: the cache falls back and the client retries quietly until after().
  const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
  console.error = console.warn = console.log = console.debug = console.info = () => {};

  const { default: prisma, SQL_NOW, sqlUtc, disconnect } = await import('../src/database/client.js');
  const { Prisma } = await import('@prisma/client');

  after(async () => {
    try { (await import('../src/services/redis.js')).default.disconnect(); } catch { /* ignore */ }
    try { await disconnect(); } catch { /* ignore */ }
    Object.assign(console, quiet);
  });

  /** SQLSTATE of a failed raw query (Prisma reports it as meta.code, or inside the message), else null. */
  function sqlstate(err) {
    const direct = err?.meta?.code;
    if (typeof direct === 'string' && /^[0-9A-Z]{5}$/.test(direct)) return direct;
    const m = /Code: `([0-9A-Z]{5})`/.exec(String(err?.message ?? ''));
    return m ? m[1] : null;
  }

  // Class 42 = syntax error / undefined table, column or function; class 22 = data exception (bad cast, out of range).
  // Class 23 (integrity violation on an empty database) still proves the statement parsed, planned and ran.
  const FATAL_CLASSES = ['42', '22'];
  // `SELECT pg_advisory_xact_lock(...)` returns void, which $queryRaw cannot deserialize (production runs it with
  // $executeRaw). The statement itself ran fine, so that one client-side error counts as executed.
  const VOID_RESULT = /Failed to deserialize column of type 'void'/;
  const isFatal = (err) => {
    if (!err) return false;
    const state = sqlstate(err);
    if (state === null) return !VOID_RESULT.test(String(err.message ?? ''));
    return FATAL_CLASSES.includes(state.slice(0, 2));
  };

  // ── Collect every RAW_SQL export ───────────────────────────────────────────
  const files = walk(SRC).filter((f) => /export\s+const\s+RAW_SQL\b/.test(readFileSync(f, 'utf8')));
  const entries = [];
  for (const file of files) {
    const ns = await import(pathToFileURL(file).href);
    assert.ok(Array.isArray(ns.RAW_SQL), `${rel(file)} must export RAW_SQL as an array`);
    for (const item of ns.RAW_SQL) entries.push({ file: rel(file), name: item?.name, build: item?.build });
  }

  // ── Run them, each behind its own savepoint, inside one transaction that is rolled back ──
  const ROLLBACK = new Error('rollback');
  const outcomes = new Map();
  const probes = {};
  try {
    await prisma.$transaction(async (tx) => {
      // The sample statements use guild '0'; several insert rows that reference GuildSettings.
      await tx.guildSettings.upsert({ where: { guildId: '0' }, update: {}, create: { guildId: '0' } });
      const attempt = async (build) => {
        await tx.$executeRawUnsafe('SAVEPOINT raw_sql');
        let error = null;
        try { await tx.$queryRaw(build()); } catch (err) { error = err; }
        await tx.$executeRawUnsafe('ROLLBACK TO SAVEPOINT raw_sql');
        return error;
      };
      // Harness self-checks: the classifier must see a syntax error and a data exception.
      probes.undefinedTable = await attempt(() => Prisma.sql`SELECT 1 FROM "NoSuchTable_db_test"`);
      probes.badCast = await attempt(() => Prisma.sql`SELECT 'not a number'::int`);
      probes.integrity = await attempt(() => Prisma.sql`INSERT INTO "UserLevel" ("id","guildId","userId","updatedAt") VALUES ('db-test','no-such-guild','0', ${SQL_NOW})`);
      for (const entry of entries) {
        outcomes.set(entry, typeof entry.build === 'function' ? await attempt(entry.build) : new TypeError('build is not a function'));
      }

      // SQL_NOW is UTC whatever the session time zone is.
      await tx.$executeRawUnsafe(`SET LOCAL TimeZone = 'America/New_York'`);
      const [{ now }] = await tx.$queryRaw(Prisma.sql`SELECT ${SQL_NOW} AS now`);
      probes.sqlNow = now;
      const sample = new Date(Date.UTC(2026, 0, 2, 3, 4, 5, 6));
      const [{ ts }] = await tx.$queryRaw(Prisma.sql`SELECT ${sqlUtc(sample)} AS ts`);
      probes.sqlUtc = { sample, ts };
      throw ROLLBACK;
    }, { timeout: 120_000, maxWait: 30_000 });
  } catch (err) {
    if (err !== ROLLBACK) throw err;
  }

  // ── Assertions ─────────────────────────────────────────────────────────────
  test('D0 the harness finds the RAW_SQL exports and each one is a { name, build } entry', () => {
    assert.ok(files.length >= 10, `expected the services to export RAW_SQL, found ${files.length} files`);
    const names = entries.map((e) => e.name);
    assert.deepEqual(entries.filter((e) => typeof e.name !== 'string' || !e.name || typeof e.build !== 'function').map((e) => `${e.file}:${e.name}`), []);
    assert.equal(new Set(names).size, names.length, 'RAW_SQL names must be unique across modules');
  });

  test('D0 the error classifier sees syntax errors and data exceptions, and tolerates integrity violations', () => {
    assert.equal(sqlstate(probes.undefinedTable)?.slice(0, 2), '42');
    assert.equal(sqlstate(probes.badCast)?.slice(0, 2), '22');
    assert.equal(isFatal(probes.undefinedTable), true);
    assert.equal(isFatal(probes.badCast), true);
    assert.equal(sqlstate(probes.integrity)?.slice(0, 2), '23');
    assert.equal(isFatal(probes.integrity), false);
    assert.equal(isFatal(null), false);
  });

  for (const entry of entries) {
    test(`D1 RAW_SQL ${entry.name} (${entry.file}) executes against the migrated schema`, () => {
      const err = outcomes.get(entry);
      assert.equal(isFatal(err), false, `${entry.name}: SQLSTATE ${err && sqlstate(err)} ${String(err?.message ?? '').split('\n').slice(-2).join(' ')}`);
    });
  }

  test('D2 SQL_NOW is UTC and independent of the session time zone', () => {
    const drift = Math.abs(probes.sqlNow.getTime() - Date.now());
    assert.ok(drift < 5000, `SQL_NOW is ${drift} ms away from Date.now() under TimeZone America/New_York`);
  });

  test('D3 sqlUtc round-trips a Date through timestamp-without-time-zone', () => {
    assert.equal(probes.sqlUtc.ts.getTime(), probes.sqlUtc.sample.getTime());
  });
}
