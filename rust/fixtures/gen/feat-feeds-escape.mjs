// Writes rust/fixtures/golden/feat-feeds/escape.json: feedService's escapeThirdParty (discord.js escapeMarkdown with
// maskedLink, heading, bulletedList and numberedList) on inputs with code blocks, inline code, lists and links, plus
// generated mixes (fixed seed, so the file only changes when the JS does).
import { writeFileSync, mkdirSync } from 'node:fs';
import { escapeMarkdown } from 'discord.js';

const opts = { maskedLink: true, heading: true, bulletedList: true, numberedList: true };
const inputs = [
  '# Title',
  '- item\n* item2\n1. one',
  '[a](http://x)',
  'before\n```\n# not a heading\n- list\n```\n# heading after',
  '```js\n[link](http://y)\n```',
  'text `# code` and\n# real',
  '```# x```',
  '  ## two\n   ### three',
  '`- a`\n- b',
  'a ```b\n# c``` d\n1. e',
  '**bold** _it_ __u__ ~~s~~ ||sp|| \\ back',
  '```\nunterminated\n# h',
];
let seed = 7;
const rnd = (n) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n; };
const bits = ['# ', '## ', '### ', '#### ', '- ', '* ', '1. ', '12. ', '[a](b)', '[x](', '```', '`', '**', '_', '__', '~~', '||', '\\',
  '\n', '\n  ', 'text', ' ', '> ', '-# ', '<t:1>', 'é', '😀', '*a*', '\n- ', '\n1. ', '](', '['];
for (let i = 0; i < 300; i++) {
  let t = '';
  for (let j = 0, n = 1 + rnd(7); j < n; j++) t += bits[rnd(bits.length)];
  inputs.push(t);
}
const out = inputs.map((input) => ({ input, output: escapeMarkdown(input, opts) }));
mkdirSync('rust/fixtures/golden/feat-feeds', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-feeds/escape.json', `${JSON.stringify(out, null, 1)}\n`);
console.log(`wrote ${out.length} cases`);
