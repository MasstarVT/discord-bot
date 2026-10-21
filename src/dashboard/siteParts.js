// Shared pieces of the public pages (landing, /docs, /changelog): escaping, the footer contact and support links,
// each catalog category's flavour (its ink on the pages), and how a setting's default is shown. Pure: no I/O.

const ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };

/** The public GitHub repository, its issue tracker and its changelog on GitHub. */
export const REPO_URL = 'https://github.com/MasstarVT/discord-bot';
export const ISSUES_URL = `${REPO_URL}/issues`;

/** Escapes text for HTML content and double-quoted attributes. */
export function escapeHtml(value) {
  return String(value ?? '').replace(/[&<>"']/g, (c) => ESCAPES[c]);
}

/** Each catalog category's flavour: the class suffix the pages colour it by, and its name on the box. */
export const FLAVOURS = Object.freeze({
  safety: Object.freeze({ cls: 'cherry', name: 'Cherry' }),
  community: Object.freeze({ cls: 'lemon', name: 'Lemon' }),
  engagement: Object.freeze({ cls: 'lime', name: 'Lime' }),
  tools: Object.freeze({ cls: 'grape', name: 'Grape' }),
});

/** The flavour of a category id (an unknown one gets the first flavour rather than no colour). */
export function flavourOf(categoryId) {
  return FLAVOURS[categoryId] ?? FLAVOURS.safety;
}

/** Who may run a command, as the pages say it. */
export const WHO_LABEL = Object.freeze({ everyone: 'Everyone', moderators: 'Moderators', admins: 'Admins', owner: 'Bot owner' });

/** A command's description without the "(MEE6 ...)" parity note the command files carry. */
export function publicDescription(text) {
  return String(text ?? '').replace(/\s*\(MEE6[^)]*\)\.?$/, '').trim();
}

/** A settings hint without the sentences comparing it to MEE6 ("MEE6 uses 15."), which are notes for admins, not
 * claims for the public pages. */
export function publicNote(text) {
  return String(text ?? '').split(/(?<=\.)\s+/).filter((s) => !/\bMEE6\b/.test(s)).join(' ').trim();
}

/** The footer "Contact" link; only a plausible email address or an http(s) URL becomes one. */
export function renderContact(contact) {
  const value = typeof contact?.value === 'string' ? contact.value.trim() : '';
  if (contact?.kind === 'email' && /^[^\s@<>"'?#]+@[^\s@<>"'?#]+$/.test(value)) {
    return `<a href="mailto:${escapeHtml(value)}">Contact</a>`;
  }
  if (contact?.kind === 'url' && /^https?:\/\//i.test(value)) return `<a href="${escapeHtml(value)}">Contact</a>`;
  return '';
}

/** The "Support server" link (DASHBOARD_SUPPORT_URL), or nothing. */
export function renderSupport(supportUrl, label = 'Support server') {
  const value = typeof supportUrl === 'string' ? supportUrl.trim() : '';
  return /^https:\/\//i.test(value) ? `<a href="${escapeHtml(value)}">${escapeHtml(label)}</a>` : '';
}

/**
 * Inline markdown for changelog lines: `code`, **bold** and [text](https://...) links; everything else is escaped.
 * @param {string} text
 * @returns {string} HTML
 */
export function inlineMarkdown(text) {
  const out = [];
  const re = /`([^`]+)`|\*\*([^*]+)\*\*|\[([^\]]+)\]\((https:\/\/[^)\s]+)\)/g;
  let last = 0;
  for (const m of String(text ?? '').matchAll(re)) {
    out.push(escapeHtml(text.slice(last, m.index)));
    if (m[1] !== undefined) out.push(`<code>${escapeHtml(m[1])}</code>`);
    else if (m[2] !== undefined) out.push(`<strong>${escapeHtml(m[2])}</strong>`);
    else out.push(`<a href="${escapeHtml(m[4])}">${escapeHtml(m[3])}</a>`);
    last = m.index + m[0].length;
  }
  out.push(escapeHtml(String(text ?? '').slice(last)));
  return out.join('');
}

/** Field types whose default reads as one short value. */
const SCALAR_TYPES = new Set(['int', 'float', 'boolean', 'enum', 'text']);

/**
 * How a settings field's default is shown, or null when it has no short value (lists, channels, roles, objects).
 * @param {{ type: string, default?: unknown, unit?: string, options?: { value: string, label: string }[] }} field
 * @returns {string|null}
 */
export function formatDefault(field) {
  if (!SCALAR_TYPES.has(field.type)) return null;
  const d = field.default;
  if (field.type === 'boolean') return d ? 'On' : 'Off';
  if (field.type === 'enum') return field.options?.find((o) => o.value === d)?.label ?? null;
  if (d === null || d === undefined || d === '') return null;
  if (field.type === 'text') return String(d).length <= 80 ? String(d) : null;
  // a unit the label already names ("Cooldown (seconds)") is not repeated after the value
  return field.unit && !String(field.label ?? '').toLowerCase().includes(field.unit) ? `${d} ${field.unit}` : String(d);
}

/** Every settings field of a catalog module that has a short default, in display order. */
export function scalarSettings(mod) {
  return mod.groups.flatMap((g) => g.fields)
    .map((f) => ({ field: f, value: formatDefault(f) }))
    .filter((s) => s.value !== null);
}
