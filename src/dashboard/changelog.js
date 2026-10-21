// The public changelog (/changelog) and the landing page's latest entries, from CHANGELOG.md at the repo root. Only the
// shape that file uses is read: "## YYYY-MM-DD · Title" headings, "- " items, and the paragraph under the "# " title.
// Pure: no I/O (server.js reads the file).
import { escapeHtml, inlineMarkdown } from './siteParts.js';

const ENTRY_RE = /^##\s+(\d{4}-\d{2}-\d{2})\s+·\s+(.+)$/;

/**
 * @param {string} markdown contents of CHANGELOG.md
 * @returns {{ intro: string, entries: { date: string, title: string, items: string[] }[] }}
 */
export function parseChangelog(markdown) {
  const intro = [];
  const entries = [];
  for (const raw of String(markdown ?? '').split(/\r?\n/)) {
    const line = raw.trim();
    const heading = ENTRY_RE.exec(line);
    if (heading) entries.push({ date: heading[1], title: heading[2].trim(), items: [] });
    else if (line.startsWith('- ') && entries.length) entries.at(-1).items.push(line.slice(2).trim());
    else if (line && !line.startsWith('#') && !entries.length) intro.push(line);
  }
  return { intro: intro.join(' '), entries };
}

/** The anchor of an entry on /changelog. */
export const entryId = (date) => `lot-${date}`;

/** The landing page's "packed on" list: the newest entries, linked to /changelog. */
export function renderLatestChanges(entries, count = 3) {
  return entries.slice(0, count).map((e) => [
    '<li class="lot">',
    `<time class="lot-date" datetime="${escapeHtml(e.date)}">${escapeHtml(e.date)}</time>`,
    `<a class="lot-title" href="/changelog#${escapeHtml(entryId(e.date))}">${escapeHtml(e.title)}</a>`,
    '</li>',
  ].join('')).join('\n');
}

/** The full changelog: one block per dated entry; its lot stamp sits in a gutter beside the title (under it on phones). */
export function renderChangelogEntries(entries) {
  if (!entries.length) return '<p class="leaf-empty">No entries yet.</p>';
  return entries.map((e) => [
    `<article class="lot-entry" id="${escapeHtml(entryId(e.date))}">`,
    `<h2 class="lot-name">${escapeHtml(e.title)}</h2>`,
    `<p class="lot-stamp">LOT <time datetime="${escapeHtml(e.date)}">${escapeHtml(e.date)}</time></p>`,
    `<ul class="lot-items">${e.items.map((item) => `<li>${inlineMarkdown(item)}</li>`).join('')}</ul>`,
    '</article>',
  ].join('')).join('\n');
}
