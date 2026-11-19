// Mock database for testing the Rust port: a real PostgreSQL 17 (portable binaries from the embedded-postgres npm
// package, nothing installed system-wide) and a Redis-protocol mock (redis-mock.mjs), both on 127.0.0.1 only.
//
//   node rust/mockdb/mockdb.mjs start            start both servers; build the template database on first run
//   node rust/mockdb/mockdb.mjs db <name>        fresh database <name> cloned from the template; prints its env lines
//   node rust/mockdb/mockdb.mjs env <name>       print the env lines again (no reset)
//   node rust/mockdb/mockdb.mjs rebuild          rebuild the template (after a new migration or seed change)
//   node rust/mockdb/mockdb.mjs status | stop
//
// MOCKDB_PG_URL (for example postgresql://postgres:ci@127.0.0.1:5432) uses a PostgreSQL server that is already running
// instead of starting the bundled one (CI does this with the postgres:17 image); the URL has no database name.
//
// The template (bot_template) is created by the JS toolchain: `prisma migrate deploy` applies prisma/migrations, then
// seed.mjs writes two rows into every table through the Prisma client. Each test database is a copy of it, and each
// name also gets its own Redis database number, so parallel test runs never see each other's data.
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, openSync, readFileSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import pg from 'pg';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const PLATFORM = `${process.platform === 'win32' ? 'windows' : process.platform}-${process.arch}`;
const BIN = join(HERE, 'node_modules', '@embedded-postgres', PLATFORM, 'native', 'bin');
const EXTERNAL_PG = (process.env.MOCKDB_PG_URL ?? '').replace(/\/+$/, '');
const DATA = join(HERE, 'data', 'pg');
const STATE_FILE = join(HERE, 'state.json');
const PG_PORT = 54329;
const REDIS_PORT = 63799;
const TEMPLATE = 'bot_template';
const pgUrl = (database) => (EXTERNAL_PG ? `${EXTERNAL_PG}/${database}` : `postgresql://postgres:mock@127.0.0.1:${PG_PORT}/${database}`);

const say = (line) => process.stdout.write(`${line}\n`);
const readState = () => { try { return JSON.parse(readFileSync(STATE_FILE, 'utf8')); } catch { return { redisDbs: {} }; } };
const writeState = (s) => writeFileSync(STATE_FILE, `${JSON.stringify(s, null, 2)}\n`);
const exe = (name) => join(BIN, process.platform === 'win32' ? `${name}.exe` : name);

function portOpen(port) {
  return new Promise((done) => {
    const s = net.connect({ host: '127.0.0.1', port });
    s.once('connect', () => { s.destroy(); done(true); });
    s.once('error', () => done(false));
    s.setTimeout(1000, () => { s.destroy(); done(false); });
  });
}
async function waitForPort(port, ms) {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    if (await portOpen(port)) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}
async function admin(fn, database = 'postgres') {
  const client = new pg.Client({ connectionString: pgUrl(database) });
  await client.connect();
  try { return await fn(client); } finally { await client.end(); }
}
const safeName = (name) => {
  if (!/^[a-z][a-z0-9_]{0,50}$/.test(name ?? '')) throw new Error('database name: lowercase letters, digits and _ only (start with a letter)');
  return name;
};

async function startPostgres() {
  if (EXTERNAL_PG) {
    for (let i = 0; i < 150; i++) {
      try { await admin((c) => c.query('select 1')); return; } catch { await new Promise((r) => setTimeout(r, 200)); }
    }
    throw new Error('the PostgreSQL server of MOCKDB_PG_URL is not answering');
  }
  if (await portOpen(PG_PORT)) return;
  if (!existsSync(join(DATA, 'PG_VERSION'))) {
    mkdirSync(dirname(DATA), { recursive: true });
    const init = spawnSync(exe('initdb'), ['-D', DATA, '-U', 'postgres', '-A', 'trust', '-E', 'UTF8', '--locale-provider=icu', '--icu-locale=en-US', '--locale=C'], { encoding: 'utf8' });
    if (init.status !== 0) throw new Error(`initdb failed:\n${init.stdout}\n${init.stderr}`);
  }
  // Start through pg_ctl. Spawning postgres.exe from Node as a detached, hidden process gives every backend its own
  // hidden console, which made each new connection take 0.3 to 9 s instead of 30 ms and timed tests out.
  const opts = `-p ${PG_PORT} -c listen_addresses=127.0.0.1 -c max_connections=400 -c fsync=off -c synchronous_commit=off -c full_page_writes=off -c timezone=UTC`;
  const started = spawnSync(exe('pg_ctl'), ['-D', DATA, '-l', join(HERE, 'postgres.log'), '-o', opts, '-W', 'start'], { stdio: 'ignore', windowsHide: true });
  if (started.status !== 0) throw new Error('pg_ctl start failed; see rust/mockdb/postgres.log');
  if (!(await waitForPort(PG_PORT, 30_000))) throw new Error('PostgreSQL did not start; see rust/mockdb/postgres.log');
  // Accepting TCP is not yet accepting queries.
  for (let i = 0; i < 50; i++) {
    try { await admin((c) => c.query('select 1')); return; } catch { await new Promise((r) => setTimeout(r, 200)); }
  }
  throw new Error('PostgreSQL is not answering; see rust/mockdb/postgres.log');
}

async function startRedis() {
  if (await portOpen(REDIS_PORT)) return;
  const log = openSync(join(HERE, 'redis-mock.log'), 'a');
  const child = spawn(process.execPath, [join(HERE, 'redis-mock.mjs'), String(REDIS_PORT)], { detached: true, stdio: ['ignore', log, log], windowsHide: true });
  child.unref();
  const state = readState();
  state.redisPid = child.pid;
  writeState(state);
  if (!(await waitForPort(REDIS_PORT, 10_000))) throw new Error('redis-mock did not start; see rust/mockdb/redis-mock.log');
}

async function buildTemplate({ force }) {
  const exists = await admin(async (c) => (await c.query('select 1 from pg_database where datname = $1', [TEMPLATE])).rowCount > 0);
  if (exists && !force) return;
  await admin(async (c) => {
    await c.query(`drop database if exists ${TEMPLATE} with (force)`);
    await c.query(`create database ${TEMPLATE}`);
  });
  const env = { ...process.env, DATABASE_URL: pgUrl(TEMPLATE) };
  const migrate = spawnSync(process.execPath, [join(ROOT, 'node_modules', 'prisma', 'build', 'index.js'), 'migrate', 'deploy', '--schema', join(ROOT, 'prisma', 'schema.prisma')], { cwd: ROOT, env, encoding: 'utf8' });
  if (migrate.status !== 0) throw new Error(`prisma migrate deploy failed:\n${migrate.stdout}\n${migrate.stderr}`);
  say(migrate.stdout.trim().split('\n').slice(-2).join('\n'));
  const seed = spawnSync(process.execPath, [join(HERE, 'seed.mjs')], { cwd: ROOT, env, encoding: 'utf8' });
  if (seed.status !== 0) throw new Error(`seed failed:\n${seed.stdout}\n${seed.stderr}`);
  say(seed.stdout.trim());
}

function envLines(name) {
  const state = readState();
  if (!(name in state.redisDbs)) {
    state.redisDbs[name] = Object.keys(state.redisDbs).length + 1; // 0 stays free for ad-hoc use
    writeState(state);
  }
  return [
    `export TEST_DATABASE_URL='${pgUrl(name)}'`,
    `export TEST_REDIS_URL='redis://127.0.0.1:${REDIS_PORT}/${state.redisDbs[name]}'`,
  ].join('\n');
}

const [cmd, arg] = process.argv.slice(2);
try {
  if (cmd === 'start') {
    await startPostgres();
    await startRedis();
    await buildTemplate({ force: false });
    say(`PostgreSQL ${EXTERNAL_PG ? 'from MOCKDB_PG_URL' : `17 on 127.0.0.1:${PG_PORT}`} (template database ${TEMPLATE}), redis-mock on 127.0.0.1:${REDIS_PORT}`);
  } else if (cmd === 'rebuild') {
    await startPostgres();
    await buildTemplate({ force: true });
  } else if (cmd === 'db') {
    const name = safeName(arg);
    if (name === TEMPLATE || name === 'postgres') throw new Error('that name is reserved');
    await startPostgres();
    await startRedis();
    await buildTemplate({ force: false });
    await admin(async (c) => {
      await c.query(`drop database if exists ${name} with (force)`);
      // `db <name> empty` gives a database with no tables (for testing the migration runner from scratch).
      await c.query(process.argv[4] === 'empty' ? `create database ${name}` : `create database ${name} template ${TEMPLATE}`);
    });
    const lines = envLines(name);
    // A fresh database gets a fresh cache too: stale cached settings would point at rows that no longer exist.
    const n = String(readState().redisDbs[name]);
    await new Promise((done, fail) => {
      const s = net.connect({ host: '127.0.0.1', port: REDIS_PORT }, () => s.write(`*2\r\n$6\r\nSELECT\r\n$${n.length}\r\n${n}\r\n*1\r\n$7\r\nFLUSHDB\r\n`));
      let seen = '';
      s.on('data', (d) => { seen += d; if (seen.split('\r\n').length > 2) { s.destroy(); done(); } });
      s.once('error', fail);
    });
    say(lines);
  } else if (cmd === 'env') {
    say(envLines(safeName(arg)));
  } else if (cmd === 'status') {
    if (EXTERNAL_PG) { say('postgres: external (MOCKDB_PG_URL)'); }
    else say(`postgres: ${(await portOpen(PG_PORT)) ? 'up' : 'down'} (127.0.0.1:${PG_PORT})`);
    say(`redis-mock: ${(await portOpen(REDIS_PORT)) ? 'up' : 'down'} (127.0.0.1:${REDIS_PORT})`);
    if (EXTERNAL_PG || (await portOpen(PG_PORT))) say(`databases: ${(await admin((c) => c.query("select datname from pg_database where not datistemplate and datname <> 'postgres' order by 1"))).rows.map((r) => r.datname).join(' ')}`);
  } else if (cmd === 'stop') {
    if (!EXTERNAL_PG && (await portOpen(PG_PORT))) spawnSync(exe('pg_ctl'), ['-D', DATA, '-m', 'fast', '-w', 'stop'], { stdio: 'ignore' });
    const { redisPid } = readState();
    if (redisPid && (await portOpen(REDIS_PORT))) { try { process.kill(redisPid); } catch { /* already gone */ } }
    say('stopped');
  } else {
    say('usage: node rust/mockdb/mockdb.mjs start | db <name> | env <name> | rebuild | status | stop');
    process.exitCode = 2;
  }
} catch (err) {
  process.stderr.write(`${err.message}\n`);
  process.exitCode = 1;
}
