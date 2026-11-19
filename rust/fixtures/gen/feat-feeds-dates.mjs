// Writes rust/fixtures/golden/feat-feeds/dates-fuzz.json: Date.parse on generated date strings (RFC 822 / 1123 variants,
// ISO 8601 edge cases, two-digit years, named and numeric zones, out-of-range fields), as V8 reads them. Run with TZ=UTC
// (the script sets it), because a string without a zone is read in the local zone. Fixed seed.
process.env.TZ = 'UTC';
import { writeFileSync, mkdirSync } from 'node:fs';

let seed = 0x1234567;
const rnd = (n) => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) % n; };
const pick = (a) => a[rnd(a.length)];
const pad = (n, w = 2) => String(n).padStart(w, '0');

const days = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun', 'Monday', 'thu', 'FRI', 'Xyz', ''];
const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec', 'January', 'sept', 'OCT', 'Foo'];
const zones = ['GMT', 'UTC', 'UT', 'Z', 'EST', 'EDT', 'CST', 'CDT', 'MST', 'MDT', 'PST', 'PDT', '+0000', '-0500', '+0530', '+05:30', '-12:00',
  '+1400', 'A', 'z', '', 'CET', 'BST', 'GMT+2', 'GMT-0700', '+5', '-0', 'UTC+01:00', '(PST)', 'utc'];
const years = [2026, 1999, 26, 99, 70, 49, 50, 0, 1, 10000, 275760, 1969, 2000, 3];
const seps = [' ', ', ', ',', '  ', '-', '/', 'T', '\t'];

const out = new Set();
for (let i = 0; i < 500; i++) {
  const d = 1 + rnd(33);
  const h = rnd(26);
  const m = rnd(61);
  const s = rnd(62);
  const y = pick(years);
  const mo = pick(months);
  const z = pick(zones);
  const form = rnd(10);
  let str;
  if (form === 0) str = `${pick(days)}${pick(seps)}${d} ${mo} ${y} ${pad(h)}:${pad(m)}:${pad(s)} ${z}`;
  else if (form === 1) str = `${pick(days)}, ${pad(d)} ${mo} ${y} ${pad(h)}:${pad(m)} ${z}`;
  else if (form === 2) str = `${y}-${pad(1 + rnd(13))}-${pad(d)}T${pad(h)}:${pad(m)}:${pad(s)}${pick(['', '.1', '.123', '.123456', 'Z', '.5Z', '+01:00', '-0800', 'z', ' Z'])}`;
  else if (form === 3) str = `${y}-${pad(1 + rnd(12))}-${pad(d)}${pick(['', ' ', 'T'])}${pick(['', `${pad(h)}:${pad(m)}`, `${h}:${m}`])}`;
  else if (form === 4) str = `${mo} ${d}, ${y} ${h}:${pad(m)}${pick(['', ' AM', ' PM', 'pm', ' am'])} ${z}`;
  else if (form === 5) str = `${pad(1 + rnd(13))}/${pad(d)}/${y}${pick(['', ` ${pad(h)}:${pad(m)}`])}`;
  else if (form === 6) str = `${d} ${mo} ${y}${pick(['', ` ${pad(h)}:${pad(m)}:${pad(s)}`])}${pick(['', ` ${z}`])}`;
  else if (form === 7) str = `${pick(['+', '-', ''])}${pick(['002026', '275760', '000000', '2026'])}-${pad(1 + rnd(12))}-${pad(d)}`;
  else if (form === 8) str = `${pick(days)} ${mo} ${pad(d)} ${pad(h)}:${pad(m)}:${pad(s)} ${y}`;
  else str = pick(['', ' ', 'now', 'today', '2026', '2026-10', '10/2026', 'Oct 2026', '1e3', '12345', 'T10:00', '2026-10-09T', '2026-10-09T25:00',
    '2026-02-30', '2026-02-29', '2024-02-29', 'Sat, 31 Dec 2016 23:59:60 GMT', '2026-10-09 10:00:00 +0000', '9 Oct 2026', 'Oct 9 2026 10:00 GMT+0200 (CEST)']);
  out.add(str);
}
const result = {};
for (const s of out) {
  const t = Date.parse(s);
  result[s] = Number.isFinite(t) ? t : null;
}
mkdirSync('rust/fixtures/golden/feat-feeds', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-feeds/dates-fuzz.json', `${JSON.stringify(result, null, 1)}\n`);
console.log(`wrote ${Object.keys(result).length} dates`);
