// The JS side of feat-tags' cross-language tests: the JS bot's own tagService (and Prisma, Redis) against the test
// servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   pure <json [[fn, ...args], ...]>        pure tagService functions (no servers): one result per call
//   create-tag <json data>                  createTagRow(data)
//   find-tag <guildId> <name>               findTag
//   write-tag <guildId> <name> <json patch> writeTag(guildId, name, () => patch)
//   create-trigger <json data>              createTriggerRow(data)
//   trigger-set <guildId>                   getTriggerSet in a fresh process (Redis, then Postgres) -> projections
//   cache-get <key> / cache-set <key> <json> <ttl>
//   resolve <guildId> <input>               resolveTrigger -> row, or { error, code }
//   number <guildId> <triggerId>            triggerNumber
//   trigger-search <guildId> <q>            the /tag trigger autocomplete query (Prisma) -> ids
//   name-search <guildId> <q>               the /tag name autocomplete query (Prisma) -> names
//   acquire <key> <seconds>                 acquireCooldown
//   uses <tagId>                            incrementTagUses, then the row's uses and updatedAt
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
const [cmd, ...args] = process.argv.slice(2);
const offline = cmd === 'pure';
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = (!offline && process.env.TEST_DATABASE_URL) || 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = (!offline && process.env.TEST_REDIS_URL) || 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'tagService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis, cache } = await imp('src', 'services', 'redis.js');
if (!offline) await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

const scope = (c) => (c ? { id: c.id, isThread: () => c.thread === true, parentId: c.parentId ?? null, parent: { parentId: c.parentParentId ?? null } } : null);
const member = (roles) => (roles ? { roles: { cache: new Set(roles) } } : null);

function call([fn, ...a]) {
  switch (fn) {
    case 'checkTagAccess': return svc.checkTagAccess(a[0], { member: member(a[1]), channel: scope(a[2]) });
    case 'checkTagRoles': return svc.checkTagRoles(a[0], member(a[1]));
    case 'channelScopeIds': return svc.channelScopeIds(scope(a[0]));
    case 'triggerAppliesHere': return svc.triggerAppliesHere(a[0], { channel: scope(a[1]), member: member(a[2]) });
    case 'buildUsage': return svc.buildUsage(a[0], a[1]);
    case 'describeAction': return svc.describeAction(a[0], null);
    case 'actionLabel': return svc.actionLabel(a[0], null);
    case 'pickResponse': return svc.pickResponse(a[0], () => a[1]);
    case 'planRun': {
      const plan = svc.planRun(a[0], { args: a[1] }, () => a[2]);
      return { steps: plan.steps.map((s) => s.response ?? null), missing: plan.missing };
    }
    case 'buildMessagePayload': {
      const p = svc.buildMessagePayload(a[0], a[1], { user: a[2] ? { id: a[2], username: 'ann', tag: 'ann' } : undefined }, a[3], a[4] ?? {});
      if (!p) return null;
      const embeds = (p.embeds ?? []).map((e) => {
        const d = e.toJSON ? e.toJSON() : e;
        const { timestamp, ...rest } = d;
        return { ...rest, hasTimestamp: Boolean(timestamp) };
      });
      return { content: p.content ?? null, embeds, allowedMentions: p.allowedMentions };
    }
    case 'validateTriggerPattern': return svc.validateTriggerPattern(...a);
    case 'compiles': return svc.compiledFor({ id: 'x', v: String(Math.random()), trigger: a[0], caseSensitive: a[1] }) !== null;
    default: {
      const f = svc[fn];
      if (typeof f !== 'function') throw new Error(`no function ${fn}`);
      const r = f(...a);
      return r === undefined ? null : r;
    }
  }
}

try {
  if (cmd === 'pure') {
    const out = [];
    for (const c of JSON.parse(args[0])) {
      // ['primeTriggers', projections] once, then ['selectTrigger', content, channel, roles] against that memo.
      if (c[0] === 'primeTriggers') {
        svc.__testing.reset();
        svc.__testing.primeTriggers('g', c[1]);
        out.push(null);
        continue;
      }
      if (c[0] === 'selectTrigger') {
        const set = await svc.getTriggerSet('g');
        out.push(svc.selectTrigger(set.entries, c[1], { channel: scope(c[2]), member: member(c[3]) })?.id ?? null);
        continue;
      }
      try {
        out.push(call(c));
      } catch (err) {
        out.push({ threw: String(err?.message ?? err) });
      }
    }
    answer(out);
  } else if (cmd === 'create-tag') {
    answer(await svc.createTagRow(JSON.parse(args[0])));
  } else if (cmd === 'find-tag') {
    answer(await svc.findTag(args[0], args[1]));
  } else if (cmd === 'write-tag') {
    const patch = JSON.parse(args[2]);
    answer(await svc.writeTag(args[0], args[1], () => patch));
  } else if (cmd === 'create-trigger') {
    answer(await svc.createTriggerRow(JSON.parse(args[0])));
  } else if (cmd === 'trigger-set') {
    const set = await svc.getTriggerSet(args[0]);
    answer(set ? set.entries.map((e) => ({ ...e.t, compiled: e.re !== null })) : null);
  } else if (cmd === 'cache-get') {
    answer(await cache.get(args[0]));
  } else if (cmd === 'cache-set') {
    answer(await cache.set(args[0], JSON.parse(args[1]), Number(args[2])));
  } else if (cmd === 'resolve') {
    try {
      answer(await svc.resolveTrigger(args[0], args[1]));
    } catch (err) {
      answer({ error: err.message, code: err.code ?? null, title: err.title ?? null });
    }
  } else if (cmd === 'number') {
    const row = await prisma.autoTrigger.findUnique({ where: { id: args[1] } });
    answer(await svc.triggerNumber(args[0], row));
  } else if (cmd === 'trigger-search') {
    const q = args[1].trim();
    const rows = await prisma.autoTrigger.findMany({
      where: {
        guildId: args[0],
        ...(q ? { OR: [{ trigger: { contains: q, mode: 'insensitive' } }, { response: { contains: q, mode: 'insensitive' } }, { id: { startsWith: q } }] } : {}),
      },
      orderBy: [{ createdAt: 'asc' }, { id: 'asc' }],
      take: 25,
      select: { id: true },
    });
    answer(rows.map((r) => r.id));
  } else if (cmd === 'name-search') {
    const q = svc.normalizeTagName(args[1]);
    const rows = await prisma.customTag.findMany({
      where: { guildId: args[0], ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}) },
      orderBy: [{ uses: 'desc' }, { name: 'asc' }],
      take: 25,
      select: { name: true },
    });
    answer(rows.map((r) => r.name));
  } else if (cmd === 'acquire') {
    answer(await svc.acquireCooldown(args[0], Number(args[1])));
  } else if (cmd === 'uses') {
    await svc.incrementTagUses(args[0]);
    const row = await prisma.customTag.findUnique({ where: { id: args[0] } });
    answer({ uses: row?.uses ?? null, updatedAt: row?.updatedAt ?? null });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err), code: err?.code ?? null });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
