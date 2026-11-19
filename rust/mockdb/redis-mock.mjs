// A small Redis-protocol (RESP2) server for tests. It is NOT Redis: it implements the commands the bot uses (strings with
// EX/PX/NX, counters, expiry, sorted sets, lists, hashes, SCAN/KEYS globs, UNLINK, MULTI/EXEC, SELECT) with Redis's reply
// formats, so the JS bot (ioredis) and the Rust bot (redis crate) can talk to the same keyspace over TCP.
// Usage: node redis-mock.mjs [port]      (default 63799, 127.0.0.1 only)
import net from 'node:net';

const PORT = Number(process.argv[2] ?? 63799);
const dbs = new Map(); // index -> Map(key -> { type, value, exp })
const db = (i) => { if (!dbs.has(i)) dbs.set(i, new Map()); return dbs.get(i); };

class RedisError extends Error {}
const WRONGTYPE = () => new RedisError('WRONGTYPE Operation against a key holding the wrong kind of value');
const NOTINT = () => new RedisError('ERR value is not an integer or out of range');
const SYNTAX = () => new RedisError('ERR syntax error');
const OK = { simple: 'OK' };

function enc(v) {
  if (v === null || v === undefined) return '$-1\r\n';
  if (v instanceof RedisError) return `-${v.message}\r\n`;
  if (typeof v === 'number') return `:${Math.trunc(v)}\r\n`;
  if (typeof v === 'string') return `$${Buffer.byteLength(v)}\r\n${v}\r\n`;
  if (Array.isArray(v)) return `*${v.length}\r\n${v.map(enc).join('')}`;
  if (v.simple) return `+${v.simple}\r\n`;
  if (v.nilArray) return '*-1\r\n';
  throw new Error('unencodable reply');
}

function entry(d, key, type) {
  const e = d.get(key);
  if (!e) return undefined;
  if (e.exp !== null && e.exp <= Date.now()) { d.delete(key); return undefined; }
  if (type && e.type !== type) throw WRONGTYPE();
  return e;
}
function make(d, key, type, value) {
  const e = { type, value, exp: null };
  d.set(key, e);
  return e;
}
const int = (s) => { if (!/^-?\d+$/.test(String(s))) throw NOTINT(); return Number(s); };
const float = (s) => {
  const t = String(s).toLowerCase();
  if (t === '+inf' || t === 'inf') return Infinity;
  if (t === '-inf') return -Infinity;
  const n = Number(s);
  if (s === '' || Number.isNaN(n)) throw new RedisError('ERR value is not a valid float');
  return n;
};
const fmtScore = (n) => (n === Infinity ? 'inf' : n === -Infinity ? '-inf' : String(n));
function bound(s) {
  const excl = String(s).startsWith('(');
  const n = float(excl ? String(s).slice(1) : s);
  return { n, excl };
}
const inRange = (score, lo, hi) => (lo.excl ? score > lo.n : score >= lo.n) && (hi.excl ? score < hi.n : score <= hi.n);

/** Redis glob: * ? [abc] [^abc] [a-z] and backslash escapes. */
export function globToRegExp(glob) {
  let re = '^';
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === '*') re += '[\\s\\S]*';
    else if (c === '?') re += '[\\s\\S]';
    else if (c === '\\' && i + 1 < glob.length) re += glob[++i].replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    else if (c === '[') {
      const end = glob.indexOf(']', i + 1);
      if (end === -1) re += '\\[';
      else {
        let body = glob.slice(i + 1, end);
        let neg = '';
        if (body.startsWith('^') || body.startsWith('!')) { neg = '^'; body = body.slice(1); }
        re += `[${neg}${body.replace(/[\]\\^]/g, '\\$&')}]`;
        i = end;
      }
    } else re += c.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp(`${re}$`);
}

function liveKeys(d) {
  const now = Date.now();
  const out = [];
  for (const [k, e] of d) {
    if (e.exp !== null && e.exp <= now) d.delete(k);
    else out.push(k);
  }
  return out;
}
function sortedMembers(z) {
  return [...z.entries()].sort((a, b) => a[1] - b[1] || (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
}
function sliceRange(len, start, stop) {
  let s = start < 0 ? len + start : start;
  let e = stop < 0 ? len + stop : stop;
  if (s < 0) s = 0;
  if (e >= len) e = len - 1;
  return s > e || s >= len ? null : [s, e];
}
function setExpiry(e, ms) { e.exp = ms; }

const commands = {
  PING: (c, a) => (a.length ? a[0] : { simple: 'PONG' }),
  ECHO: (c, a) => a[0],
  AUTH: () => OK,
  QUIT: (c) => { c.quit = true; return OK; },
  SELECT: (c, a) => { c.db = int(a[0]); return OK; },
  HELLO: (c, a) => {
    if (a.length && a[0] !== '2') return new RedisError('NOPROTO unsupported protocol version');
    return ['server', 'redis', 'version', '7.4.0', 'proto', 2, 'id', c.id, 'mode', 'standalone', 'role', 'master', 'modules', []];
  },
  CLIENT: (c, a) => {
    const sub = (a[0] ?? '').toUpperCase();
    if (sub === 'ID') return c.id;
    if (sub === 'GETNAME') return c.name ?? null;
    if (sub === 'SETNAME') { c.name = a[1]; return OK; }
    return OK;
  },
  INFO: () => '# Server\r\nredis_version:7.4.0\r\nredis_mode:standalone\r\nmock:1\r\n# Persistence\r\nloading:0\r\n# Replication\r\nrole:master\r\n',
  COMMAND: () => [],
  CONFIG: (c, a) => ((a[0] ?? '').toUpperCase() === 'GET' ? [] : OK),
  TIME: () => { const now = Date.now(); return [String(Math.floor(now / 1000)), String((now % 1000) * 1000)]; },
  DBSIZE: (c) => liveKeys(db(c.db)).length,
  FLUSHDB: (c) => { db(c.db).clear(); return OK; },
  FLUSHALL: () => { dbs.clear(); return OK; },
  WATCH: () => OK,
  UNWATCH: () => OK,

  // ── strings ──
  GET: (c, a) => entry(db(c.db), a[0], 'string')?.value ?? null,
  GETDEL: (c, a) => { const d = db(c.db); const v = entry(d, a[0], 'string')?.value ?? null; d.delete(a[0]); return v; },
  MGET: (c, a) => a.map((k) => { const e = entry(db(c.db), k); return e && e.type === 'string' ? e.value : null; }),
  MSET: (c, a) => { for (let i = 0; i + 1 < a.length; i += 2) make(db(c.db), a[i], 'string', a[i + 1]); return OK; },
  STRLEN: (c, a) => Buffer.byteLength(entry(db(c.db), a[0], 'string')?.value ?? ''),
  APPEND: (c, a) => {
    const d = db(c.db);
    const e = entry(d, a[0], 'string') ?? make(d, a[0], 'string', '');
    e.value += a[1];
    return Buffer.byteLength(e.value);
  },
  SET: (c, a) => {
    const d = db(c.db);
    const [key, value, ...opts] = a;
    if (key === undefined || value === undefined) throw new RedisError("ERR wrong number of arguments for 'set' command");
    let exp = null; let nx = false; let xx = false; let keepTtl = false; let get = false;
    for (let i = 0; i < opts.length; i++) {
      const o = opts[i].toUpperCase();
      if (o === 'NX') nx = true;
      else if (o === 'XX') xx = true;
      else if (o === 'KEEPTTL') keepTtl = true;
      else if (o === 'GET') get = true;
      else if (o === 'EX' || o === 'PX' || o === 'EXAT' || o === 'PXAT') {
        const n = int(opts[++i]);
        if (n <= 0) throw new RedisError("ERR invalid expire time in 'set' command");
        exp = o === 'EX' ? Date.now() + n * 1000 : o === 'PX' ? Date.now() + n : o === 'EXAT' ? n * 1000 : n;
      } else throw SYNTAX();
    }
    if (nx && xx) throw SYNTAX();
    const old = entry(d, key);
    if (get && old && old.type !== 'string') throw WRONGTYPE();
    const previous = old ? old.value : null;
    if ((nx && old) || (xx && !old)) return get ? previous : null;
    const e = make(d, key, 'string', value);
    e.exp = keepTtl && old ? old.exp : exp;
    return get ? previous : OK;
  },
  SETNX: (c, a) => { const d = db(c.db); if (entry(d, a[0])) return 0; make(d, a[0], 'string', a[1]); return 1; },
  SETEX: (c, a) => { const n = int(a[1]); if (n <= 0) throw new RedisError("ERR invalid expire time in 'setex' command"); make(db(c.db), a[0], 'string', a[2]).exp = Date.now() + n * 1000; return OK; },
  PSETEX: (c, a) => { const n = int(a[1]); if (n <= 0) throw new RedisError("ERR invalid expire time in 'psetex' command"); make(db(c.db), a[0], 'string', a[2]).exp = Date.now() + n; return OK; },
  INCR: (c, a) => incrBy(c, a[0], 1),
  DECR: (c, a) => incrBy(c, a[0], -1),
  INCRBY: (c, a) => incrBy(c, a[0], int(a[1])),
  DECRBY: (c, a) => incrBy(c, a[0], -int(a[1])),

  // ── keys ──
  DEL: (c, a) => del(c, a),
  UNLINK: (c, a) => del(c, a),
  EXISTS: (c, a) => a.filter((k) => entry(db(c.db), k)).length,
  TYPE: (c, a) => ({ simple: entry(db(c.db), a[0])?.type ?? 'none' }),
  EXPIRE: (c, a) => expire(c, a[0], Date.now() + int(a[1]) * 1000),
  PEXPIRE: (c, a) => expire(c, a[0], Date.now() + int(a[1])),
  EXPIREAT: (c, a) => expire(c, a[0], int(a[1]) * 1000),
  PEXPIREAT: (c, a) => expire(c, a[0], int(a[1])),
  PERSIST: (c, a) => { const e = entry(db(c.db), a[0]); if (!e || e.exp === null) return 0; e.exp = null; return 1; },
  TTL: (c, a) => { const e = entry(db(c.db), a[0]); return !e ? -2 : e.exp === null ? -1 : Math.max(0, Math.round((e.exp - Date.now()) / 1000)); },
  PTTL: (c, a) => { const e = entry(db(c.db), a[0]); return !e ? -2 : e.exp === null ? -1 : Math.max(0, e.exp - Date.now()); },
  RENAME: (c, a) => { const d = db(c.db); const e = entry(d, a[0]); if (!e) throw new RedisError('ERR no such key'); d.delete(a[0]); d.set(a[1], e); return OK; },
  KEYS: (c, a) => { const re = globToRegExp(a[0]); return liveKeys(db(c.db)).filter((k) => re.test(k)); },
  SCAN: (c, a) => {
    const cursor = int(a[0]);
    let match = null; let count = 10; let type = null;
    for (let i = 1; i < a.length; i += 2) {
      const o = a[i].toUpperCase();
      if (o === 'MATCH') match = globToRegExp(a[i + 1]);
      else if (o === 'COUNT') count = Math.max(1, int(a[i + 1]));
      else if (o === 'TYPE') type = a[i + 1];
      else throw SYNTAX();
    }
    const d = db(c.db);
    const keys = liveKeys(d);
    const page = keys.slice(cursor, cursor + count).filter((k) => (!match || match.test(k)) && (!type || d.get(k).type === type));
    const next = cursor + count >= keys.length ? 0 : cursor + count;
    return [String(next), page];
  },

  // ── sorted sets ──
  ZADD: (c, a) => {
    const d = db(c.db);
    const key = a[0];
    let i = 1; let nx = false; let xx = false; let gt = false; let lt = false; let ch = false;
    for (; i < a.length; i++) {
      const o = a[i].toUpperCase();
      if (o === 'NX') nx = true; else if (o === 'XX') xx = true; else if (o === 'GT') gt = true; else if (o === 'LT') lt = true; else if (o === 'CH') ch = true;
      else break;
    }
    if ((a.length - i) % 2 !== 0 || a.length - i === 0) throw SYNTAX();
    const pairs = [];
    for (; i < a.length; i += 2) pairs.push([float(a[i]), a[i + 1]]);
    const e = entry(d, key, 'zset') ?? make(d, key, 'zset', new Map());
    let added = 0; let changed = 0;
    for (const [score, member] of pairs) {
      const had = e.value.has(member);
      if (had) {
        const old = e.value.get(member);
        if (nx || (gt && score <= old) || (lt && score >= old) || old === score) continue;
        e.value.set(member, score); changed++;
      } else if (!xx) { e.value.set(member, score); added++; }
    }
    if (e.value.size === 0) d.delete(key);
    return ch ? added + changed : added;
  },
  ZCARD: (c, a) => entry(db(c.db), a[0], 'zset')?.value.size ?? 0,
  ZSCORE: (c, a) => { const s = entry(db(c.db), a[0], 'zset')?.value.get(a[1]); return s === undefined ? null : fmtScore(s); },
  ZCOUNT: (c, a) => { const lo = bound(a[1]); const hi = bound(a[2]); return [...(entry(db(c.db), a[0], 'zset')?.value.values() ?? [])].filter((s) => inRange(s, lo, hi)).length; },
  ZREM: (c, a) => {
    const d = db(c.db); const e = entry(d, a[0], 'zset'); if (!e) return 0;
    let n = 0; for (const m of a.slice(1)) if (e.value.delete(m)) n++;
    if (e.value.size === 0) d.delete(a[0]);
    return n;
  },
  ZREMRANGEBYSCORE: (c, a) => {
    const d = db(c.db); const e = entry(d, a[0], 'zset'); if (!e) return 0;
    const lo = bound(a[1]); const hi = bound(a[2]);
    let n = 0; for (const [m, s] of [...e.value]) if (inRange(s, lo, hi)) { e.value.delete(m); n++; }
    if (e.value.size === 0) d.delete(a[0]);
    return n;
  },
  ZRANGE: (c, a) => {
    const e = entry(db(c.db), a[0], 'zset'); if (!e) return [];
    const withScores = a.slice(3).some((o) => o.toUpperCase() === 'WITHSCORES');
    const all = sortedMembers(e.value);
    const r = sliceRange(all.length, int(a[1]), int(a[2]));
    if (!r) return [];
    return all.slice(r[0], r[1] + 1).flatMap(([m, s]) => (withScores ? [m, fmtScore(s)] : [m]));
  },
  ZRANGEBYSCORE: (c, a) => {
    const e = entry(db(c.db), a[0], 'zset'); if (!e) return [];
    const lo = bound(a[1]); const hi = bound(a[2]);
    const withScores = a.slice(3).some((o) => o.toUpperCase() === 'WITHSCORES');
    return sortedMembers(e.value).filter(([, s]) => inRange(s, lo, hi)).flatMap(([m, s]) => (withScores ? [m, fmtScore(s)] : [m]));
  },

  // ── lists ──
  LPUSH: (c, a) => { const d = db(c.db); const e = entry(d, a[0], 'list') ?? make(d, a[0], 'list', []); for (const v of a.slice(1)) e.value.unshift(v); return e.value.length; },
  RPUSH: (c, a) => { const d = db(c.db); const e = entry(d, a[0], 'list') ?? make(d, a[0], 'list', []); e.value.push(...a.slice(1)); return e.value.length; },
  LLEN: (c, a) => entry(db(c.db), a[0], 'list')?.value.length ?? 0,
  LRANGE: (c, a) => { const e = entry(db(c.db), a[0], 'list'); if (!e) return []; const r = sliceRange(e.value.length, int(a[1]), int(a[2])); return r ? e.value.slice(r[0], r[1] + 1) : []; },
  LTRIM: (c, a) => {
    const d = db(c.db); const e = entry(d, a[0], 'list'); if (!e) return OK;
    const r = sliceRange(e.value.length, int(a[1]), int(a[2]));
    e.value = r ? e.value.slice(r[0], r[1] + 1) : [];
    if (e.value.length === 0) d.delete(a[0]);
    return OK;
  },
  LPOP: (c, a) => { const d = db(c.db); const e = entry(d, a[0], 'list'); if (!e) return null; const v = e.value.shift(); if (!e.value.length) d.delete(a[0]); return v; },
  RPOP: (c, a) => { const d = db(c.db); const e = entry(d, a[0], 'list'); if (!e) return null; const v = e.value.pop(); if (!e.value.length) d.delete(a[0]); return v; },

  // ── hashes ──
  HSET: (c, a) => {
    if (a.length < 3 || a.length % 2 === 0) throw new RedisError("ERR wrong number of arguments for 'hset' command");
    const d = db(c.db); const e = entry(d, a[0], 'hash') ?? make(d, a[0], 'hash', new Map());
    let n = 0; for (let i = 1; i < a.length; i += 2) { if (!e.value.has(a[i])) n++; e.value.set(a[i], a[i + 1]); }
    return n;
  },
  HGET: (c, a) => entry(db(c.db), a[0], 'hash')?.value.get(a[1]) ?? null,
  HMGET: (c, a) => { const e = entry(db(c.db), a[0], 'hash'); return a.slice(1).map((f) => e?.value.get(f) ?? null); },
  HDEL: (c, a) => {
    const d = db(c.db); const e = entry(d, a[0], 'hash'); if (!e) return 0;
    let n = 0; for (const f of a.slice(1)) if (e.value.delete(f)) n++;
    if (e.value.size === 0) d.delete(a[0]);
    return n;
  },
  HGETALL: (c, a) => [...(entry(db(c.db), a[0], 'hash')?.value.entries() ?? [])].flat(),
  HEXISTS: (c, a) => (entry(db(c.db), a[0], 'hash')?.value.has(a[1]) ? 1 : 0),
  HLEN: (c, a) => entry(db(c.db), a[0], 'hash')?.value.size ?? 0,
};

function incrBy(c, key, by) {
  const d = db(c.db);
  const e = entry(d, key, 'string');
  if (!e) { make(d, key, 'string', String(by)); return by; }
  const n = int(e.value) + by;
  if (!Number.isSafeInteger(n)) throw new RedisError('ERR increment or decrement would overflow');
  e.value = String(n);
  return n;
}
function del(c, keys) {
  const d = db(c.db);
  let n = 0;
  for (const k of keys) if (entry(d, k) && d.delete(k)) n++;
  return n;
}
function expire(c, key, at) {
  const d = db(c.db);
  const e = entry(d, key);
  if (!e) return 0;
  if (at <= Date.now()) d.delete(key); else setExpiry(e, at);
  return 1;
}

function run(c, name, args) {
  const fn = commands[name];
  if (!fn) return new RedisError(`ERR unknown command '${name.toLowerCase()}', with args beginning with: ${args.slice(0, 3).map((x) => `'${x}'`).join(' ')}`);
  try { return fn(c, args); } catch (err) {
    if (err instanceof RedisError) return err;
    return new RedisError(`ERR ${err.message}`);
  }
}

function dispatch(c, argv) {
  const name = argv[0].toUpperCase();
  const args = argv.slice(1);
  if (name === 'MULTI') {
    if (c.multi) return new RedisError('ERR MULTI calls can not be nested');
    c.multi = []; c.dirty = false; return OK;
  }
  if (name === 'DISCARD') {
    if (!c.multi) return new RedisError('ERR DISCARD without MULTI');
    c.multi = null; return OK;
  }
  if (name === 'EXEC') {
    if (!c.multi) return new RedisError('ERR EXEC without MULTI');
    const queued = c.multi; c.multi = null;
    if (c.dirty) return new RedisError('EXECABORT Transaction discarded because of previous errors.');
    return queued.map(([n, a]) => run(c, n, a));
  }
  if (c.multi) {
    if (!commands[name]) { c.dirty = true; return new RedisError(`ERR unknown command '${name.toLowerCase()}'`); }
    c.multi.push([name, args]);
    return { simple: 'QUEUED' };
  }
  return run(c, name, args);
}

/** Parses one command from buf at offset; returns { argv, next } or null when incomplete. */
function parse(buf, offset) {
  const lineEnd = buf.indexOf('\r\n', offset);
  if (lineEnd === -1) return null;
  if (buf[offset] !== 0x2a /* * */) {
    const line = buf.toString('utf8', offset, lineEnd).trim();
    return { argv: line ? line.split(/\s+/) : [], next: lineEnd + 2 };
  }
  const n = Number(buf.toString('latin1', offset + 1, lineEnd));
  let pos = lineEnd + 2;
  const argv = [];
  for (let i = 0; i < n; i++) {
    const e = buf.indexOf('\r\n', pos);
    if (e === -1) return null;
    const len = Number(buf.toString('latin1', pos + 1, e));
    const start = e + 2;
    if (buf.length < start + len + 2) return null;
    argv.push(buf.toString('utf8', start, start + len));
    pos = start + len + 2;
  }
  return { argv, next: pos };
}

let nextId = 1;
const server = net.createServer((socket) => {
  const c = { id: nextId++, db: 0, multi: null, dirty: false, quit: false, name: null };
  let buf = Buffer.alloc(0);
  socket.setNoDelay(true);
  socket.on('data', (chunk) => {
    buf = buf.length ? Buffer.concat([buf, chunk]) : chunk;
    let offset = 0;
    let out = '';
    for (;;) {
      const cmd = parse(buf, offset);
      if (!cmd) break;
      offset = cmd.next;
      if (cmd.argv.length === 0) continue;
      out += enc(dispatch(c, cmd.argv));
      if (c.quit) break;
    }
    buf = buf.subarray(offset);
    if (out) socket.write(out);
    if (c.quit) socket.end();
  });
  socket.on('error', () => {});
});
server.listen(PORT, '127.0.0.1', () => process.stdout.write(`redis-mock listening on 127.0.0.1:${PORT}\n`));
