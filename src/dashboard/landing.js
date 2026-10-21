// The public pages: the landing page (GET /, public/landing.html), the module and command reference (/docs,
// public/docs.html), the changelog (/changelog, public/changelog.html) and the header and footer of the legal pages
// (/privacy, /terms). Each template is filled with data from the
// dashboard catalog, src/dashboard/commands.json and CHANGELOG.md, so a module or command added to the bot shows up on
// the pages by itself. Pure: no I/O (server.js reads the templates and CHANGELOG.md).
//
// Markers, all replaced in one pass (inserted text is never scanned again); an unknown marker throws:
//   every page:   <!--module-count--> <!--command-count--> <!--contact--> (footer "Contact" link or nothing)
//                 <!--support--> (a "Support server" link or nothing) <!--code:KEY--> (module KEY's two-letter code)
//                 <!--flavour:KEY--> (module KEY's flavour class suffix) @@INVITE_URL@@ (the bot's invite link,
//                 HTML-escaped, or /dashboard when none is configured)
//   landing:      <!--module-table--> <!--bot-facts--> <!--ingredients--> <!--latest-changes--> <!--levels-demo-->
//   docs:         <!--docs-toc--> <!--docs-modules--> <!--docs-core-->
//   changelog:    <!--changelog-intro--> <!--changelog-->
//   legal pages:  only the shared ones (server.js fills their <!--contact--> with the contact text first)
import { buildCatalog } from './catalog.js';
import COMMANDS from './commands.json' with { type: 'json' };
import { parseChangelog, renderChangelogEntries, renderLatestChanges } from './changelog.js';
import { commandId, moduleId, renderCoreCommands, renderDocsModules, renderDocsToc } from './docs.js';
import {
  WHO_LABEL, escapeHtml, flavourOf, inlineMarkdown, publicDescription, publicNote, renderContact, renderSupport, scalarSettings,
} from './siteParts.js';

/** Where "Add to Discord" points when no invite link is configured. */
const FALLBACK_INVITE = '/dashboard';

/** Settings a module wrapper lists before pointing at /docs for the rest. */
const WRAPPER_SETTINGS = 4;

const MARKERS = /<!--([a-z]+(?:-[a-z]+)*)(?::([^>]*?))?-->|@@INVITE_URL@@/g;

/** "1 module" / "4 modules". */
function countLabel(n) {
  return `${n} ${n === 1 ? 'module' : 'modules'}`;
}

/** The category id of every module. */
function categoryByModule(catalog) {
  return new Map(catalog.categories.flatMap((c) => c.modules.map((k) => [k, c.id])));
}

/**
 * The "what's inside" box: per category a flavour heading and a run of identical wrappers, each a <details> that opens
 * to the module's switch, setup command, commands and first few settings.
 * @param {ReturnType<typeof buildCatalog>} catalog
 * @returns {string}
 */
function renderModuleTable(catalog) {
  return catalog.categories.map((category) => {
    const flavour = flavourOf(category.id);
    const pops = category.modules.map((key) => {
      const mod = catalog.modules[key];
      const cmds = COMMANDS.filter((c) => c.module === key);
      const settings = scalarSettings(mod).slice(0, WRAPPER_SETTINGS);
      const enable = mod.default
        ? `<p class="pop-line">On when the bot joins. Off: <code>/module disable ${escapeHtml(key)}</code></p>`
        : `<p class="pop-line">Switch it on: <code>/module enable ${escapeHtml(key)}</code></p>`;
      const setup = mod.configCommand ? `<p class="pop-line">Set it up: <code>${escapeHtml(mod.configCommand)}</code></p>` : '';
      const cmdList = cmds.length
        ? `<p class="pop-sub">Commands</p><ul class="pop-cmds">${cmds.map((c) => `<li><a href="/docs#${escapeHtml(commandId(c.name))}"><code>/${escapeHtml(c.name)}</code></a> <span>${escapeHtml(publicDescription(c.description))}</span></li>`).join('')}</ul>`
        : '';
      const settingList = settings.length
        ? `<p class="pop-sub">Settings, as they ship</p><dl class="pop-settings">${settings.map(({ field, value }) => `<div><dt>${escapeHtml(field.label)}</dt><dd>${escapeHtml(value)}</dd></div>`).join('')}</dl>`
        : '';
      return [
        `<li class="pop pop-${flavour.cls}">`,
        '<details class="pop-pack">',
        '<summary class="pop-wrapper">',
        `<span class="pop-code" aria-hidden="true">${escapeHtml(mod.code)}</span>`,
        '<span class="pop-text">',
        `<span class="pop-name">${escapeHtml(mod.label)}</span>`,
        `<span class="pop-desc">${escapeHtml(mod.description)}</span>`,
        '</span>',
        `<span class="pop-state">${mod.default ? 'On' : 'Off'}<span class="pop-state-x"> at start</span></span>`,
        '</summary>',
        '<div class="pop-inside">',
        enable,
        setup,
        cmdList,
        settingList,
        `<a class="pop-more" href="/docs#${escapeHtml(moduleId(key))}">Everything in ${escapeHtml(mod.label)}</a>`,
        '</div>',
        '</details>',
        '</li>',
      ].join('');
    });
    return [
      `<section class="flavour flavour-${flavour.cls}" aria-labelledby="flv-${escapeHtml(category.id)}">`,
      '<div class="flavour-head">',
      `<h3 class="flavour-title" id="flv-${escapeHtml(category.id)}"><span class="flavour-name">${escapeHtml(flavour.name)}</span> ${escapeHtml(category.label)}</h3>`,
      `<span class="flavour-count">${countLabel(category.modules.length)}</span>`,
      `<p class="flavour-desc">${escapeHtml(category.description)}</p>`,
      '</div>',
      `<ul class="pop-run">${pops.join('')}</ul>`,
      '</section>',
    ].join('');
  }).join('\n');
}

/** The Bot Facts panel: module counts per category and command counts per role, all counted from the data. */
function renderBotFacts(catalog) {
  const modules = Object.keys(catalog.modules).length;
  const byWho = (who) => COMMANDS.filter((c) => c.who === who).length;
  const row = (label, value, cls = '') => `<div class="facts-row${cls}"><dt>${label}</dt><dd>${value}</dd></div>`;
  return [
    '<dl class="facts-list">',
    row('Modules', String(modules), ' facts-major'),
    ...catalog.categories.map((c) => row(escapeHtml(c.label), String(c.modules.length), ' facts-minor')),
    row('Slash commands', String(COMMANDS.length), ' facts-major facts-rule'),
    ...['everyone', 'moderators', 'admins', 'owner'].filter((w) => byWho(w) > 0)
      .map((w) => row(`For ${escapeHtml(WHO_LABEL[w].toLowerCase())}`, String(byWho(w)), ' facts-minor')),
    row('Premium tiers', '0', ' facts-major facts-rule'),
    row('API keys to buy', '0', ' facts-major'),
    row('Web dashboard', 'Included', ' facts-major'),
    '</dl>',
  ].join('');
}

/** Every slash command as one ingredients line, each linked to its entry on /docs. */
function renderIngredients() {
  return COMMANDS.map((c) => `<a href="/docs#${escapeHtml(commandId(c.name))}"><code>/${escapeHtml(c.name)}</code></a>`).join(', ');
}

/** The Levels settings the landing page's sample dashboard form shows, straight from the catalog. */
function renderLevelsDemo(catalog) {
  const fields = new Map(catalog.modules.leveling.groups.flatMap((g) => g.fields).map((f) => [f.field, f]));
  const pick = (name) => {
    const f = fields.get(name);
    if (!f) throw new Error(`landing template: the Levels module has no "${name}" setting`);
    return f;
  };
  const num = (f) => [
    '<label class="ss-field">',
    `<span class="ss-label">${escapeHtml(f.label)}</span>`,
    `<input class="ss-input" type="number" name="${escapeHtml(f.field)}" value="${escapeHtml(f.default)}" min="${escapeHtml(f.min)}" max="${escapeHtml(f.max)}" inputmode="numeric">`,
    publicNote(f.description) ? `<span class="ss-note">${escapeHtml(publicNote(f.description))}</span>` : '',
    '</label>',
  ].join('');
  const mode = pick('levelUpMode');
  const embed = pick('levelUpUseEmbed');
  const message = pick('levelUpMessage');
  return [
    `<div class="ss-pair">${num(pick('xpMin'))}${num(pick('xpMax'))}</div>`,
    '<p class="ss-error" id="ss-xp-error" hidden>The maximum must be at least the minimum.</p>',
    '<fieldset class="ss-seg">',
    `<legend class="ss-label">${escapeHtml(mode.label)}</legend>`,
    mode.options.map((o) => `<label class="ss-seg-opt"><input type="radio" name="${escapeHtml(mode.field)}" value="${escapeHtml(o.value)}"${o.value === mode.default ? ' checked' : ''}><span>${escapeHtml(o.label)}</span></label>`).join(''),
    '</fieldset>',
    '<label class="ss-toggle">',
    `<input type="checkbox" role="switch" name="${escapeHtml(embed.field)}"${embed.default ? ' checked' : ''}>`,
    `<span class="ss-toggle-text"><span class="ss-label">${escapeHtml(embed.label)}</span><span class="ss-note">${escapeHtml(publicNote(embed.description))}</span></span>`,
    '</label>',
    '<label class="ss-field">',
    `<span class="ss-label">${escapeHtml(message.label)}</span>`,
    `<textarea class="ss-input ss-text" name="${escapeHtml(message.field)}" rows="2" maxlength="${escapeHtml(message.maxLength)}">${escapeHtml(message.default)}</textarea>`,
    `<span class="ss-note">Placeholders: ${(message.placeholders ?? []).map((p) => `<code>${escapeHtml(p)}</code>`).join(' ')}</span>`,
    '</label>',
  ].join('');
}

/**
 * Fills a template's markers.
 * @param {string} template
 * @param {Record<string, () => string>} pageMarkers the markers only this page has
 * @param {{ inviteUrl?: string, contact?: object, supportUrl?: string|null }} options
 * @param {ReturnType<typeof buildCatalog>} catalog
 */
function fill(template, pageMarkers, { inviteUrl, contact, supportUrl }, catalog) {
  if (typeof template !== 'string') throw new TypeError('the page template must be a string');
  const invite = escapeHtml(typeof inviteUrl === 'string' && inviteUrl.trim() ? inviteUrl.trim() : FALLBACK_INVITE);
  const categories = categoryByModule(catalog);
  const moduleArg = (name, key) => {
    if (!Object.hasOwn(catalog.modules, key)) throw new Error(`page template: unknown module "${key}" in a ${name} marker`);
    return key;
  };
  const shared = {
    'module-count': () => String(Object.keys(catalog.modules).length),
    'command-count': () => String(COMMANDS.length),
    contact: () => renderContact(contact),
    support: () => renderSupport(supportUrl),
  };
  return template.replace(MARKERS, (match, name, arg) => {
    if (match === '@@INVITE_URL@@') return invite;
    if (name === 'code' && arg !== undefined) return escapeHtml(catalog.modules[moduleArg('code', arg)].code);
    if (name === 'flavour' && arg !== undefined) return flavourOf(categories.get(moduleArg('flavour', arg))).cls;
    const handler = arg === undefined ? (pageMarkers[name] ?? shared[name]) : undefined;
    if (!handler) throw new Error(`page template: unknown marker "${match}"`);
    return handler();
  });
}

/**
 * Renders the landing page.
 * @param {string} template contents of public/landing.html
 * @param {{ inviteUrl?: string, contact?: { kind: 'email' | 'url', value: string }, supportUrl?: string|null, changelog?: string }} [options]
 *   changelog is the text of CHANGELOG.md (its newest entries are listed); without it the list is empty
 * @returns {string} the final HTML
 * @throws {Error} when the template names an unknown marker or module
 */
export function renderLanding(template, options = {}) {
  const catalog = buildCatalog();
  const { entries } = parseChangelog(options.changelog ?? '');
  return fill(template, {
    'module-table': () => renderModuleTable(catalog),
    'bot-facts': () => renderBotFacts(catalog),
    ingredients: () => renderIngredients(),
    'latest-changes': () => renderLatestChanges(entries),
    'levels-demo': () => renderLevelsDemo(catalog),
  }, options, catalog);
}

/**
 * Renders the module and command reference.
 * @param {string} template contents of public/docs.html
 * @param {{ inviteUrl?: string, contact?: object, supportUrl?: string|null }} [options]
 */
export function renderDocs(template, options = {}) {
  const catalog = buildCatalog();
  return fill(template, {
    'docs-toc': () => renderDocsToc(catalog),
    'docs-modules': () => renderDocsModules(catalog, COMMANDS),
    'docs-core': () => renderCoreCommands(COMMANDS),
  }, options, catalog);
}

/**
 * Renders a legal page's header and footer (privacy.html, terms.html); its text has no markers of its own.
 * @param {string} template the page, its <!--contact--> markers already replaced
 * @param {{ inviteUrl?: string, supportUrl?: string|null }} [options]
 */
export function renderLegal(template, options = {}) {
  return fill(template, {}, options, buildCatalog());
}

/**
 * Renders the changelog page.
 * @param {string} template contents of public/changelog.html
 * @param {{ inviteUrl?: string, contact?: object, supportUrl?: string|null, changelog?: string }} [options]
 */
export function renderChangelog(template, options = {}) {
  const catalog = buildCatalog();
  const { intro, entries } = parseChangelog(options.changelog ?? '');
  return fill(template, {
    'changelog-intro': () => inlineMarkdown(intro),
    changelog: () => renderChangelogEntries(entries),
  }, options, catalog);
}
