// Live cross-language check for rust/crates/dash-auth: seals and opens session cookies with the JS dashboard code.
// Reads a JSON array of operations on stdin and prints a JSON array of results:
//   { "op": "seal", "secret": "...", "payload": {...} }          -> { "token": "v1...." }
//   { "op": "open", "secret": "...", "token": "...", "now": n }  -> { "payload": {...} | null }
//   { "op": "cookie", "header": "..." }                          -> { "entries": [[name, value], ...] }
import { readFileSync } from 'node:fs';

const auth = await import('../../../src/dashboard/auth.js');
const ops = JSON.parse(readFileSync(0, 'utf8'));
const out = ops.map((op) => {
  if (op.op === 'seal') return { token: auth.sealSession(op.payload, auth.deriveSessionKey(op.secret)) };
  if (op.op === 'open') return { payload: auth.unsealSession(op.token, auth.deriveSessionKey(op.secret), op.now ?? Date.now()) };
  if (op.op === 'cookie') return { entries: [...auth.parseCookies(op.header)] };
  throw new Error(`unknown op ${op.op}`);
});
process.stdout.write(JSON.stringify(out));
process.exit(0);
