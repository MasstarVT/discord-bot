// escapeMarkdown(text, options) of @discordjs/formatters (what discord.js re-exports), over inputs x option sets,
// for bot_core::markdown::escape_markdown_with.
//   node rust/fixtures/gen/bot-core-markdown.mjs   -> rust/fixtures/golden/bot-core/markdown.json
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { escapeMarkdown } from 'discord.js';

const OUT = join(resolve(dirname(fileURLToPath(import.meta.url)), '..'), 'golden', 'bot-core', 'markdown.json');

const inputs = [
  'plain text',
  '*italic* and **bold** and ***both***',
  '_under_ __underline__ ___both___',
  '`code` and ``double`` and ```block```',
  '```js\nconst a = 1 * 2;\n```\nafter *star*',
  'text `inline *star*` more _x_',
  '~~strike~~ ||spoiler|| back\\slash',
  '# Heading\n## Sub\n  ### three\n- bullet\n* star bullet\n  - nested\n1. one\n 22. two',
  '[label](https://example.com) and [a](b)',
  'https://example.com/some_path_with*stars*_ and <https://x.y/a_b>',
  '<:name_with_underscore:123456789012345678> and <a:anim_x:123456789012345678>',
  'snake_case_word and __init__ and a_b_c',
  '**unclosed bold and *unclosed italic',
  'mixed ***_ __ ** _ * ___ markers',
  '',
];

const optionSets = [
  {},
  { codeBlockContent: false },
  { inlineCodeContent: false },
  { codeBlockContent: false, inlineCodeContent: false },
  { heading: true, bulletedList: true, numberedList: true, maskedLink: true },
  { escape: false, italic: false, bold: false },
  { underline: false, strikethrough: false, spoiler: false, inlineCode: false, codeBlock: false },
  { codeBlock: false, codeBlockContent: false },
  { inlineCode: false, inlineCodeContent: false },
];

const cases = [];
for (const options of optionSets) {
  for (const text of inputs) cases.push({ text, options, out: escapeMarkdown(text, options) });
}
mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(OUT, `${JSON.stringify(cases, null, 1)}\n`);
console.log(`wrote ${OUT}: ${cases.length} cases`);
