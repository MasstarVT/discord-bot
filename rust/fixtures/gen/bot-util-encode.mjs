// Shared by the bot-util fixture generators: encodes JS values as JSON the Rust tests decode (see tests/common).
// undefined → {$u}, -0/NaN/±Infinity → {$n}, BigInt → {$big}, Date → {$d: ms}, RegExp → {$re, flags}, Buffer → {$buf: base64},
// Error → {$err, name, code}, a string with a lone surrogate → {$u16: [code units]}; discord.js-like objects in a placeholder context become plain data ({$ctx}).
export class Unreplayable extends Error {}

const IMAGE_OPTS = { extension: 'png', size: 256 };

export function enc(v, depth = 0) {
  if (depth > 12) throw new Unreplayable('deep');
  if (v === undefined) return { $u: 1 };
  if (typeof v === 'string') return v.isWellFormed() ? v : { $u16: Array.from({ length: v.length }, (_, i) => v.charCodeAt(i)) };
  if (v === null || typeof v === 'boolean') return v;
  if (typeof v === 'number') return Number.isFinite(v) && !Object.is(v, -0) ? v : { $n: Object.is(v, -0) ? '-0' : String(v) };
  if (typeof v === 'bigint') return { $big: v.toString() };
  if (typeof v === 'function' || typeof v === 'symbol') throw new Unreplayable('fn');
  if (v instanceof Date) return { $d: v.getTime() };
  if (v instanceof RegExp) return { $re: v.source, flags: v.flags };
  if (Buffer.isBuffer(v) || v instanceof Uint8Array) return { $buf: Buffer.from(v.buffer, v.byteOffset, v.byteLength).toString('base64') };
  if (v instanceof Error) return { $err: v.message, name: v.name, code: v.code === undefined ? { $u: 1 } : enc(v.code, depth + 1) };
  if (Array.isArray(v)) return v.map((x) => enc(x, depth + 1));
  if (v instanceof Map || v instanceof Set) throw new Unreplayable('collection');
  const proto = Object.getPrototypeOf(v);
  if (proto !== Object.prototype && proto !== null) throw new Unreplayable('class');
  const out = {};
  for (const [k, x] of Object.entries(v)) out[k] = enc(x, depth + 1);
  return out;
}

// What placeholderParser reads from discord.js objects, as plain data (the Rust RenderContext).
function plainUser(u) {
  if (u == null) return null;
  return {
    id: enc(u.id), username: enc(u.username), tag: enc(u.tag), discriminator: enc(u.discriminator), globalName: enc(u.globalName),
    bot: Boolean(u.bot), createdTimestamp: enc(u.createdTimestamp), avatarURL: enc(u.displayAvatarURL?.(IMAGE_OPTS)),
  };
}
function plainGuild(g) {
  if (g == null) return null;
  return {
    id: enc(g.id), name: enc(g.name), ownerId: enc(g.ownerId), memberCount: enc(g.memberCount), verificationLevel: enc(g.verificationLevel),
    joinedTimestamp: enc(g.joinedTimestamp), createdTimestamp: enc(g.createdTimestamp),
    premiumSubscriptionCount: enc(g.premiumSubscriptionCount), premiumTier: enc(g.premiumTier), iconURL: enc(g.iconURL?.(IMAGE_OPTS)),
  };
}
export function plainCtx(c) {
  if (!c || typeof c !== 'object') return enc(c);
  const m = c.member ?? null;
  const vars = {};
  if (c.vars && typeof c.vars === 'object') {
    for (const [k, v] of Object.entries(c.vars)) {
      if (typeof v === 'function') {
        try { const r = v(); vars[k] = { $fnres: r === null || r === undefined ? null : String(r) }; } catch (e) { vars[k] = { $fnthrow: String(e?.message) }; }
      } else vars[k] = enc(v);
    }
  }
  return {
    $ctx: 1,
    member: m && {
      displayName: enc(m.displayName), joinedTimestamp: enc(m.joinedTimestamp), avatarURL: enc(m.displayAvatarURL?.(IMAGE_OPTS)),
      user: plainUser(m.user), guild: plainGuild(m.guild),
    },
    user: plainUser(c.user), guild: plainGuild(c.guild),
    channel: c.channel == null ? null : { id: enc(c.channel.id), name: enc(c.channel.name), type: enc(c.channel.type) },
    vars: c.vars && typeof c.vars === 'object' ? vars : null,
    args: Array.isArray(c.args) ? c.args.map((a) => String(a ?? '')) : null,
  };
}

