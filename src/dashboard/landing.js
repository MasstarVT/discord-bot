// The public landing page (GET /): fills src/dashboard/public/landing.html with data from the dashboard catalog, so a
// module added to src/dashboard/catalog.js shows up on the page by itself. Pure: no I/O (server.js reads the template).
//
// Markers in the template, all replaced in one pass (inserted text is never scanned again):
//   <!--module-count-->  the number of modules
//   <!--module-table-->  one block per catalog category: heading, module count, description and a grid of module cards
//   <!--code:KEY-->      the two-letter code of module KEY (an unknown KEY throws)
//   @@INVITE_URL@@       the bot's invite link (HTML-escaped), or /dashboard when none is configured
//   <!--contact-->       a footer "Contact" link, or nothing
import { buildCatalog } from './catalog.js';

const ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };

/** Where "Add to Discord" points when no invite link is configured. */
const FALLBACK_INVITE = '/dashboard';

const MARKERS = /<!--(module-count|module-table|contact)-->|<!--code:([^>]*?)-->|@@INVITE_URL@@/g;

/** Escapes text for HTML content and double-quoted attributes. */
function escapeHtml(value) {
  return String(value ?? '').replace(/[&<>"']/g, (c) => ESCAPES[c]);
}

/** "1 module" / "4 modules". */
function countLabel(n) {
  return `${n} ${n === 1 ? 'module' : 'modules'}`;
}

/**
 * The module table: per category a heading row and a grid of cards, numbered 01.. across the whole table.
 * @param {ReturnType<typeof buildCatalog>} catalog
 * @returns {string}
 */
function renderModuleTable(catalog) {
  let n = 0;
  return catalog.categories.map((category) => {
    const cards = category.modules.map((key) => {
      const mod = catalog.modules[key];
      n += 1;
      return [
        '<li class="mod">',
        '<div class="mod-top">',
        `<span class="mod-code" aria-hidden="true">${escapeHtml(mod.code)}</span>`,
        `<span class="mod-num" aria-hidden="true">${String(n).padStart(2, '0')}</span>`,
        '</div>',
        `<h4 class="mod-name">${escapeHtml(mod.label)}</h4>`,
        `<p class="mod-desc">${escapeHtml(mod.description)}</p>`,
        '</li>',
      ].join('');
    });
    return [
      '<div class="cat">',
      '<div class="cat-head">',
      `<h3 class="cat-title">${escapeHtml(category.label)}</h3>`,
      `<span class="cat-count">${countLabel(category.modules.length)}</span>`,
      `<p class="cat-desc">${escapeHtml(category.description)}</p>`,
      '</div>',
      `<ul class="mod-grid">${cards.join('')}</ul>`,
      '</div>',
    ].join('');
  }).join('\n');
}

/** The footer contact link; only a plausible email address or an http(s) URL becomes one. */
function renderContact(contact) {
  const value = typeof contact?.value === 'string' ? contact.value.trim() : '';
  if (contact?.kind === 'email' && /^[^\s@<>"'?#]+@[^\s@<>"'?#]+$/.test(value)) {
    return `<a href="mailto:${escapeHtml(value)}">Contact</a>`;
  }
  if (contact?.kind === 'url' && /^https?:\/\//i.test(value)) return `<a href="${escapeHtml(value)}">Contact</a>`;
  return '';
}

/**
 * Renders the landing page.
 * @param {string} template contents of public/landing.html
 * @param {{ inviteUrl?: string, contact?: { kind: 'email' | 'url', value: string } }} [options]
 * @returns {string} the final HTML
 * @throws {Error} when the template names a module code that is not in the catalog
 */
export function renderLanding(template, { inviteUrl, contact } = {}) {
  if (typeof template !== 'string') throw new TypeError('renderLanding needs the template as a string');
  const catalog = buildCatalog();
  const invite = escapeHtml(typeof inviteUrl === 'string' && inviteUrl.trim() ? inviteUrl.trim() : FALLBACK_INVITE);
  return template.replace(MARKERS, (match, name, codeKey) => {
    if (match === '@@INVITE_URL@@') return invite;
    if (codeKey !== undefined) {
      if (!Object.hasOwn(catalog.modules, codeKey)) throw new Error(`landing template: unknown module "${codeKey}" in a code marker`);
      return escapeHtml(catalog.modules[codeKey].code);
    }
    if (name === 'module-count') return String(Object.keys(catalog.modules).length);
    if (name === 'module-table') return renderModuleTable(catalog);
    return renderContact(contact);
  });
}
