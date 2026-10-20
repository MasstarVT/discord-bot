// Frost.Byte dashboard: a dependency-free single-page UI (vanilla ES module, no build step), served at /dashboard.
//
// Layout of this file (shared DOM helpers, the API client, toasts and small components live in lib.js;
// field controls in forms.js, record lists and the embed editor in collections.js / embed-editor.js):
//   constants, state, the per-guild save queue, shell, screens (picker, overview, module and general
//   settings pages with the unsaved-changes bar), router, start().
//
// Module names, codes, descriptions and categories all come from /api/catalog (src/dashboard/catalog.js), so a module
// added there gets its tile, card, switch and sidebar row here without any change to this file.
//
// Nothing here touches the DOM, window or storage at import time (the module is
// import-checked in plain Node); everything runs from start() at the bottom.

import {
  TOAST_ERROR_MS,
  h, icon, pluralize, canonical, slug, safeHref, mount,
  ApiError, api, fallbackMessage,
  toast,
  initialsOf, avatar, banner, statusCard, backLink, retryButton, describeError,
  createSwitch, paintSwitch,
} from './lib.js';
import { createForm } from './forms.js';
import { mountCollection } from './collections.js';

const APP_NAME = 'Frost.Byte';
const SAVE_DEBOUNCE_MS = 120; // lets a double-click on one switch collapse into a no-op
const SAVED_TOAST_DELAY_MS = 350; // a burst of saves shows one "Saved" toast

/* ------------------------------------------------------------------ */
/* State                                                                */
/* ------------------------------------------------------------------ */

const state = {
  user: null,
  guilds: null, // /api/guilds, cached for the session
  ownerGuilds: [], // /api/guilds ownerGuilds (bot owner only)
  guildsPromise: null,
  catalog: null, // /api/catalog, cached for the session
  catalogPromise: null,
  loginError: false,
};

let routeToken = 0; // bumped on every navigation so late responses can be dropped
let currentRoute = null;
let currentView = null; // { kind, destroy, ... }; settings pages also expose isDirty() and save()
let activeShell = null;
let activeSearch = null;
let acceptedHash = ''; // the hash of the page being shown; a hashchange back to it is a cancelled navigation
let leaving = false; // set once the admin agreed to lose unsaved work (log out), so beforeunload does not ask again

function loadCatalog() {
  if (state.catalog) return Promise.resolve(state.catalog);
  if (!state.catalogPromise) {
    state.catalogPromise = api('/api/catalog').then(
      (catalog) => {
        state.catalog = catalog;
        return catalog;
      },
      (err) => {
        state.catalogPromise = null;
        throw err;
      },
    );
  }
  return state.catalogPromise;
}

function loadGuilds({ force = false } = {}) {
  if (!force && state.guilds) return Promise.resolve(state.guilds);
  if (!force && state.guildsPromise) return state.guildsPromise;
  const promise = api('/api/guilds')
    .then((data) => {
      state.guilds = Array.isArray(data?.guilds) ? data.guilds : [];
      // Only the bot owner gets ownerGuilds: every other server the bot is in.
      state.ownerGuilds = Array.isArray(data?.ownerGuilds) ? data.ownerGuilds : [];
      return state.guilds;
    })
    .finally(() => {
      if (state.guildsPromise === promise) state.guildsPromise = null;
    });
  state.guildsPromise = promise;
  return promise;
}

/** Group modules by category; modules the API forgot to categorise land in "Other". */
function normalizeCatalog(raw) {
  const modules = raw && typeof raw.modules === 'object' && raw.modules ? raw.modules : {};
  const placed = new Set();
  const categories = [];

  const pick = (keys) => {
    const picked = [];
    for (const key of Array.isArray(keys) ? keys : []) {
      const mod = modules[key];
      if (!mod || !mod.flag || placed.has(key)) continue;
      placed.add(key);
      picked.push({
        ...mod,
        key,
        label: mod.label || key,
        default: mod.default === true,
        subToggles: (Array.isArray(mod.subToggles) ? mod.subToggles : []).filter((s) => s && s.field),
      });
    }
    return picked;
  };

  for (const cat of Array.isArray(raw?.categories) ? raw.categories : []) {
    const mods = pick(cat.modules);
    if (mods.length) categories.push({ id: String(cat.id), label: cat.label || String(cat.id), description: cat.description || '', modules: mods });
  }
  const orphans = pick(Object.keys(modules));
  if (orphans.length) categories.push({ id: 'other', label: 'Other', description: '', modules: orphans });
  return { categories };
}

/** True (and the login screen is shown) when the error is a 401. */
function handleAuthError(err, notice = '') {
  if (!err || err.status !== 401) return false;
  renderLoggedOut({ notice });
  return true;
}

/* ------------------------------------------------------------------ */
/* Per-guild save queue                                                 */
/* ------------------------------------------------------------------ */

const stores = new Map();

function getStore(guildId) {
  let store = stores.get(guildId);
  if (!store) {
    store = new GuildStore(guildId);
    stores.set(guildId, store);
  }
  return store;
}

/**
 * Optimistic toggle state for one guild. `confirmed` is what the server last
 * returned; `inflight` is the request being sent; `queued` collects toggles made
 * meanwhile and goes out as ONE batched request once the current one settles.
 * At most one request per guild is ever in flight.
 */
class GuildStore {
  constructor(id) {
    this.id = id;
    this.confirmed = {};
    this.queued = new Map();
    this.inflight = new Map();
    this.running = false;
    this.timer = 0;
    this.listeners = new Set();
    this.writes = 0; // successful writes so far (a PATCH that came back, or settings the forms saved)
    this.writtenAt = new Map(); // field -> the number of the latest write that set it
  }

  /** Call just before a settings GET is sent; hand the result to load() with that response. */
  mark() {
    return this.writes;
  }

  /**
   * Takes the values of a settings GET. A response to a request sent before the latest write to a field is older than that
   * write (the PATCH may have been answered while the GET was on its way): that field keeps the value the write confirmed.
   * @param {object} values
   * @param {number|null} [since] the mark() taken before the request
   * @returns {object} the values as stored now (a copy): what a page built from this response should show
   */
  load(values, since = null) {
    const next = { ...values };
    if (since !== null) {
      for (const [field, at] of this.writtenAt) {
        if (at > since && Object.hasOwn(this.confirmed, field)) next[field] = this.confirmed[field];
      }
    }
    this.confirmed = next;
    return { ...next };
  }

  noteWrite(values) {
    this.writes += 1;
    for (const field of Object.keys(values)) this.writtenAt.set(field, this.writes);
  }

  /** Records values saved elsewhere (the settings forms) so the overview switches match. */
  confirm(values) {
    Object.assign(this.confirmed, values);
    this.noteWrite(values);
    this.emit();
  }

  get(field, fallback) {
    if (this.queued.has(field)) return this.queued.get(field);
    if (this.inflight.has(field)) return this.inflight.get(field);
    return typeof this.confirmed[field] === 'boolean' ? this.confirmed[field] : fallback;
  }

  isPending(field) {
    return this.queued.has(field) || this.inflight.has(field);
  }

  get busy() {
    return this.running || this.timer !== 0 || this.queued.size > 0;
  }

  subscribe(fn) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  emit() {
    for (const fn of this.listeners) {
      try { fn(); } catch { /* a broken view must never stall saving */ }
    }
  }

  toggle(field, fallback) {
    this.queued.set(field, !this.get(field, fallback));
    this.emit();
    if (!this.running && !this.timer) {
      this.timer = setTimeout(() => {
        this.timer = 0;
        this.flush();
      }, SAVE_DEBOUNCE_MS);
    }
  }

  async flush() {
    if (this.running) return;
    this.running = true;
    try {
      for (;;) {
        // A switch flipped back to its confirmed value needs no request.
        for (const [field, value] of this.queued) {
          if (this.confirmed[field] === value) this.queued.delete(field);
        }
        if (this.queued.size === 0) break;

        this.inflight = this.queued;
        this.queued = new Map();
        this.emit();
        try {
          const sent = Object.fromEntries(this.inflight);
          const res = await api(`/api/guilds/${encodeURIComponent(this.id)}/settings`, { method: 'PATCH', body: { changes: sent } });
          const done = res && res.values ? res.values : sent;
          Object.assign(this.confirmed, done);
          this.noteWrite(done);
          this.inflight = new Map();
          this.emit();
          noteSaved();
          showNotices(res);
        } catch (err) {
          // Roll everything not yet confirmed back to the last server state.
          this.inflight = new Map();
          this.queued = new Map();
          this.emit();
          reportSaveError(err);
          break;
        }
      }
    } finally {
      this.running = false;
      this.emit();
    }
  }
}

let savedTimer = 0;

/** Side-effect notes a save may return (e.g. "Lowered 2 balances to the new maximum."), one toast each. */
function showNotices(res) {
  const notices = res && Array.isArray(res.notices) ? res.notices : [];
  for (const notice of notices.slice(0, 3)) {
    if (typeof notice === 'string' && notice) toast(notice, { kind: 'info', ms: TOAST_ERROR_MS });
  }
}

function noteSaved() {
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => toast('Saved', { kind: 'success', key: 'saved' }), SAVED_TOAST_DELAY_MS);
}

function reportSaveError(err) {
  clearTimeout(savedTimer);
  if (err && err.status === 401) {
    renderLoggedOut({ notice: fallbackMessage(401) });
    return;
  }
  const message = (err && err.serverMessage) || fallbackMessage(err ? err.status : -1);
  toast(message, { kind: 'error', key: 'save-error', ms: TOAST_ERROR_MS });
}

/* ------------------------------------------------------------------ */
/* Shell: top bar, optional sidebar drawer, footer                      */
/* ------------------------------------------------------------------ */

/** Links to the privacy policy and terms (static pages the server serves at /privacy and /terms). */
function legalLinks(cls) {
  return h(
    'nav',
    { class: cls, 'aria-label': 'Legal' },
    h('a', { href: '/privacy' }, 'Privacy Policy'),
    ' · ',
    h('a', { href: '/terms' }, 'Terms of Service'),
  );
}

async function logout(btn) {
  // Ask before the session is revoked: if the prompt came from beforeunload after the request, "stay" would leave a
  // page whose next Save answers 401 and loses the edits.
  if (hasUnsavedWork() && !window.confirm(LEAVE_PROMPT)) return;
  btn.disabled = true;
  try {
    await api('/auth/logout', { method: 'POST', body: {} });
  } catch (err) {
    if (err.status !== 401) {
      btn.disabled = false;
      toast(err.serverMessage || 'Could not log out, try again.', { kind: 'error' });
      return;
    }
  }
  leaving = true;
  location.reload();
}

/** The Frost.Byte wordmark, linking to the landing page. */
function brand(tag = null) {
  return h(
    'a',
    { class: 'brand', href: '/' },
    h('img', { class: 'brand-mark', src: '/favicon.svg', alt: '', width: '28', height: '28' }),
    h('span', { class: 'brand-name' }, 'Frost', h('span', { class: 'brand-dot' }, '.'), 'Byte'),
    tag ? h('span', { class: 'brand-tag' }, tag) : null,
  );
}

/** The app bar's server switcher: the current server, opening the server picker. Hidden until a server is set. */
function serverSwitcher() {
  const el = h('a', { class: 'switcher', href: '#/', hidden: true });
  return {
    el,
    set(guild) {
      el.replaceChildren(
        avatar(guild.name, guild.iconUrl, { size: 'xs' }),
        h('span', { class: 'switcher-name' }, guild.name),
        h('span', { class: 'sr-only' }, ', switch server'),
        icon('chevron'),
      );
      el.hidden = false;
    },
  };
}

function buildTopbar(menuBtn, switcher) {
  const user = state.user;
  const name = user ? user.globalName || user.username || 'Account' : '';
  return h(
    'header',
    { class: 'topbar' },
    menuBtn,
    brand(switcher ? null : 'Dashboard'),
    switcher ? switcher.el : null,
    h('span', { class: 'topbar-spacer' }),
    user
      ? h('div', { class: 'user' }, avatar(name, user.avatarUrl, { size: 'sm', round: true }), h('span', { class: 'user-name' }, name))
      : null,
    user
      ? h('button', { type: 'button', class: 'link-btn', onclick: (event) => logout(event.currentTarget) }, 'Log out')
      : null,
  );
}

/**
 * Logged-in page frame. With `sidebar` it also builds the server switcher and the module sidebar,
 * which turns into a slide-in drawer under 900px.
 */
function buildShell({ sidebar = false } = {}) {
  let drawerOpen = false;
  const main = h('main', { class: 'main', id: 'main', tabindex: '-1' });
  const sectionsSlot = h('div', { class: 'side-block' });
  const switcher = sidebar ? serverSwitcher() : null;
  let menuBtn = null;
  let closeBtn = null;
  let aside = null;
  let backdrop = null;

  function setDrawer(open) {
    if (!aside || open === drawerOpen) return;
    drawerOpen = open;
    aside.classList.toggle('is-open', open);
    backdrop.classList.toggle('is-open', open);
    document.body.classList.toggle('no-scroll', open);
    menuBtn.setAttribute('aria-expanded', String(open));
    if (open) closeBtn.focus({ preventScroll: true });
    else if (aside.contains(document.activeElement)) menuBtn.focus();
  }

  if (sidebar) {
    menuBtn = h(
      'button',
      {
        type: 'button',
        class: 'icon-btn menu-btn',
        'aria-label': 'Modules',
        'aria-controls': 'sidebar',
        'aria-expanded': 'false',
        onclick: () => setDrawer(!drawerOpen),
      },
      icon('menu'),
    );
    closeBtn = h('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Close menu', onclick: () => setDrawer(false) }, icon('x'));
    aside = h(
      'aside',
      {
        class: 'sidebar',
        id: 'sidebar',
        'aria-label': 'Modules',
        onkeydown: (event) => {
          if (event.key === 'Escape' && drawerOpen) setDrawer(false);
        },
        onclick: (event) => {
          if (event.target.closest && event.target.closest('a')) setDrawer(false);
        },
      },
      h('div', { class: 'sidebar-head' }, h('span', {}, 'Menu'), closeBtn),
      sectionsSlot,
    );
    backdrop = h('div', { class: 'drawer-backdrop', onclick: () => setDrawer(false) });
  }

  const skip = h('button', { type: 'button', class: 'skip-link', onclick: () => main.focus() }, 'Skip to content');
  const footer = h(
    'footer',
    { class: 'footer' },
    h(
      'span',
      { class: 'footer-text' },
      sidebar
        ? 'Module switches save as you flip them. Other settings save with Save changes.'
        : "Don't see a server? You need the Manage Server permission there.",
    ),
    legalLinks('footer-legal'),
  );
  const root = h(
    'div',
    { class: 'app' },
    skip,
    buildTopbar(menuBtn, switcher),
    h('div', { class: sidebar ? 'layout has-sidebar' : 'layout' }, aside, h('div', { class: 'content' }, main, footer)),
    backdrop,
  );

  const shell = { root, main, sectionsSlot, setDrawer, setGuild: (guild) => switcher && switcher.set(guild) };
  activeShell = shell;
  return shell;
}

function teardownView() {
  if (currentView) currentView.destroy();
  currentView = null;
  activeSearch = null;
  if (activeShell) activeShell.setDrawer(false);
  activeShell = null;
  if (typeof document !== 'undefined') document.body.classList.remove('no-scroll', 'has-savebar');
}

/* ------------------------------------------------------------------ */
/* Screens: loading, logged out, boot error                             */
/* ------------------------------------------------------------------ */

function skeletonBlock(cls) {
  return h('span', { class: `skeleton ${cls}` });
}

function renderLoading() {
  mount(
    h(
      'div',
      { class: 'app boot' },
      h('p', { class: 'sr-only', role: 'status' }, 'Loading'),
      h(
        'div',
        { class: 'topbar', 'aria-hidden': 'true' },
        skeletonBlock('boot-logo'),
        skeletonBlock('boot-title'),
        h('span', { class: 'topbar-spacer' }),
        skeletonBlock('boot-pill'),
      ),
      h(
        'div',
        { class: 'boot-body', 'aria-hidden': 'true' },
        skeletonBlock('boot-heading'),
        h('div', { class: 'boot-grid' }, skeletonBlock('boot-card'), skeletonBlock('boot-card'), skeletonBlock('boot-card')),
      ),
    ),
  );
  document.getElementById('app').setAttribute('aria-busy', 'true');
}

function renderLoggedOut({ notice = '', error = false } = {}) {
  teardownView();
  routeToken++;
  currentRoute = null;
  state.user = null;
  document.title = APP_NAME;

  const banners = [];
  if (error) banners.push(banner('error', 'Login failed, please try again.'));
  if (notice) banners.push(banner('info', notice));

  mount(
    h(
      'div',
      { class: 'hero-screen' },
      h('header', { class: 'hero-bar' }, brand('Dashboard')),
      h(
        'main',
        { class: 'hero', id: 'main', tabindex: '-1' },
        banners,
        h(
          'div',
          { class: 'hero-card' },
          h('p', { class: 'eyebrow' }, 'FROST.BYTE DASHBOARD'),
          h('h1', { class: 'hero-title' }, 'Log in to manage your server'),
          h(
            'p',
            { class: 'hero-lead' },
            "Log in with Discord to change every module's channels, roles, messages and limits on the servers you manage.",
          ),
          h('a', { class: 'btn btn-primary btn-lg hero-cta', href: '/auth/login' }, 'Log in with Discord'),
          h('p', { class: 'hero-fine' }, 'We only read your profile and the list of your servers.'),
          legalLinks('hero-legal'),
        ),
      ),
    ),
  );
}

function renderBootError(err) {
  const info = describeError(err);
  mount(
    h(
      'div',
      { class: 'hero-screen' },
      h('header', { class: 'hero-bar' }, brand('Dashboard')),
      h(
        'main',
        { class: 'hero', id: 'main' },
        statusCard({
          tone: 'danger',
          iconName: 'alert',
          title: info.title,
          message: info.message,
          actions: [retryButton(() => boot())],
          role: 'alert',
        }),
      ),
    ),
  );
}

/* ------------------------------------------------------------------ */
/* Screen: server picker                                                */
/* ------------------------------------------------------------------ */

function pickerSkeleton() {
  return h(
    'div',
    { class: 'server-grid', 'aria-hidden': 'true' },
    [1, 2, 3, 4, 5, 6].map(() => skeletonBlock('boot-card')),
  );
}

function serverIdentity(guild, meta) {
  return h(
    'div',
    { class: 'server-identity' },
    avatar(guild.name, guild.iconUrl, { size: 'lg' }),
    h(
      'div',
      { class: 'server-text' },
      h('h3', { class: 'server-name' }, guild.name),
      h('span', { class: 'server-meta' }, meta),
      addedByLine(guild),
    ),
  );
}

/**
 * Who added the bot, for the bot owner: only their /api/guilds entries carry `addedBy` (null: nothing recorded yet;
 * a null userId: the bot could not tell, it needs View Audit Log or Manage Server there).
 */
function addedByLine(guild) {
  if (!Object.hasOwn(guild, 'addedBy')) return null;
  const info = guild.addedBy;
  if (!info) return h('span', { class: 'server-meta server-added' }, 'Added by: not recorded yet');
  const who = info.userId ? `${info.name || 'unknown name'} (${info.userId})` : 'unknown';
  let when = '';
  const date = new Date(info.joinedAt);
  if (!Number.isNaN(date.getTime())) when = ` on ${new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(date)}`;
  const hint = info.userId ? null : 'The bot needs View Audit Log or Manage Server in this server to tell who added it.';
  return h('span', { class: 'server-meta server-added', title: hint }, `Added by ${who}${when}`);
}

/** The bot owner's Join button: asks the bot for a single-use invite, then offers it as a link (no popup). */
async function requestOwnerInvite(event, guild) {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    const data = await api(`/api/guilds/${encodeURIComponent(guild.id)}/invite`, { method: 'POST' });
    const href = safeHref(data && data.url);
    if (!href) throw new ApiError(500, 'error', 'The invite could not be read. Try again.', null);
    button.replaceWith(h(
      'a',
      { class: 'btn btn-primary btn-sm', href, target: '_blank', rel: 'noopener' },
      'Open invite',
      h('span', { class: 'sr-only' }, ` to ${guild.name}`),
    ));
    toast('Invite ready. It works once and expires in 30 minutes.', { kind: 'success' });
  } catch (err) {
    button.disabled = false;
    if (handleAuthError(err)) return;
    toast(describeError(err).message, { kind: 'error', ms: TOAST_ERROR_MS });
  }
}

/** A bot-owner card for a server they are not in: Manage, and Join through an invite the bot creates. */
function ownerJoinCard(guild) {
  return h(
    'div',
    { class: 'card server-card' },
    serverIdentity(guild, 'Bot owner access, not a member'),
    h(
      'span',
      { class: 'server-buttons' },
      h('a', { class: 'btn btn-secondary btn-sm', href: `#/guild/${encodeURIComponent(guild.id)}` }, 'Manage'),
      h(
        'button',
        { class: 'btn btn-primary btn-sm', type: 'button', onclick: (event) => requestOwnerInvite(event, guild) },
        icon('plus'),
        'Join',
        h('span', { class: 'sr-only' }, ` ${guild.name}`),
      ),
    ),
  );
}

function serverCard(guild) {
  if (guild.ownerAccess && guild.member === false) return ownerJoinCard(guild);
  if (guild.botPresent) {
    return h(
      'div',
      { class: 'card server-card' },
      serverIdentity(guild, guild.ownerAccess ? 'Bot owner access' : 'Manage Server'),
      h(
        'a',
        { class: 'btn btn-primary server-manage', href: `#/guild/${encodeURIComponent(guild.id)}` },
        'Manage',
        h('span', { class: 'sr-only' }, ` ${guild.name}`),
        icon('forward'),
      ),
    );
  }
  const invite = safeHref(guild.inviteUrl);
  return h(
    'div',
    { class: 'card server-card is-dim' },
    serverIdentity(guild, 'Manage Server'),
    invite
      ? h(
        'a',
        { class: 'btn btn-secondary server-manage', href: invite, target: '_blank', rel: 'noopener' },
        icon('plus'),
        'Add Frost.Byte',
        h('span', { class: 'sr-only' }, ` to ${guild.name}`),
      )
      : null,
  );
}

/**
 * The picker's lists: servers the bot is in, servers it is not in yet and (bot owner only) every other server the bot
 * is in. Returns the filter for the search box, or null when there is nothing to list.
 */
function renderPickerBody(body, guilds, query = '') {
  const groups = [
    { label: 'FROST.BYTE IS HERE', guilds: guilds.filter((g) => g.botPresent) },
    { label: 'NOT ADDED YET', guilds: guilds.filter((g) => !g.botPresent) },
    {
      label: 'BOT OWNER ACCESS',
      note: 'Every other server the bot is in. You can manage these even where you are not an admin or a member.',
      guilds: state.ownerGuilds || [],
    },
  ].filter((g) => g.guilds.length > 0);
  if (!groups.length) {
    body.replaceChildren(
      statusCard({
        tone: 'accent',
        iconName: 'server',
        title: 'No servers to manage',
        message:
          'You need the Manage Server permission on a server to configure the bot there. Ask a server admin to grant it, then reload this page.',
        actions: [retryButton(() => showPicker())],
      }),
    );
    return null;
  }
  const sections = groups.map((group) => {
    const count = h('span', { class: 'num' });
    const cards = group.guilds.map((guild) => ({ name: String(guild.name || '').toLowerCase(), el: serverCard(guild) }));
    const el = h(
      'section',
      { class: 'picker-section', 'aria-label': group.label },
      h('h2', { class: 'section-label' }, `${group.label} · `, count),
      group.note ? h('p', { class: 'page-sub' }, group.note) : null,
      h('div', { class: 'server-grid' }, cards.map((c) => c.el)),
    );
    return { el, count, cards };
  });
  const noMatch = h('div', { class: 'empty-state', role: 'status', hidden: true });
  function filter(text) {
    const q = String(text || '').trim().toLowerCase();
    let shown = 0;
    for (const section of sections) {
      let n = 0;
      for (const card of section.cards) {
        const hit = !q || card.name.includes(q);
        card.el.hidden = !hit;
        if (hit) n++;
      }
      section.count.textContent = String(n);
      section.el.hidden = n === 0;
      shown += n;
    }
    noMatch.hidden = shown > 0;
    noMatch.textContent = shown > 0 ? '' : `No servers match “${String(text).trim()}”. Check the spelling, or make sure you have Manage Server there.`;
  }
  body.replaceChildren(...sections.map((s) => s.el), noMatch);
  filter(query);
  return filter;
}

function renderPickerError(body, err) {
  const info = describeError(err);
  body.replaceChildren(
    statusCard({
      tone: 'danger',
      iconName: 'alert',
      title: info.title,
      message: info.message,
      actions: [retryButton(() => showPicker())],
      role: 'alert',
    }),
  );
}

async function showPicker() {
  const token = ++routeToken;
  teardownView();
  document.title = APP_NAME;
  const shell = buildShell({ sidebar: false });
  mount(shell.root);
  window.scrollTo({ top: 0, behavior: 'instant' });

  let filter = null;
  const search = h('input', {
    type: 'search',
    id: 'server-search',
    class: 'search-input',
    placeholder: 'Server name',
    autocomplete: 'off',
    spellcheck: 'false',
    oninput: () => {
      if (filter) filter(search.value);
    },
  });
  const lists = h('div', { class: 'picker-lists' });
  const body = h(
    'div',
    { class: 'picker' },
    h(
      'div',
      { class: 'page-head picker-head' },
      h(
        'div',
        { class: 'page-head-text' },
        h('h1', { class: 'page-title page-title-xl' }, 'Pick a server'),
        h('p', { class: 'page-sub' }, 'These are the servers where you have Manage Server. Add Frost.Byte to a server before you change its settings.'),
      ),
      h(
        'div',
        { class: 'picker-search' },
        h('label', { class: 'field-label', for: 'server-search' }, 'Search servers'),
        h('div', { class: 'search' }, icon('search'), search),
      ),
    ),
    lists,
  );
  shell.main.append(body);
  activeSearch = search;
  if (state.guilds) filter = renderPickerBody(lists, state.guilds, search.value);
  else lists.append(pickerSkeleton());
  shell.main.focus({ preventScroll: true });

  try {
    const guilds = await loadGuilds({ force: true });
    if (token !== routeToken) return;
    filter = renderPickerBody(lists, guilds, search.value);
  } catch (err) {
    if (token !== routeToken || handleAuthError(err)) return;
    if (state.guilds) toast(describeError(err).message, { kind: 'error', ms: TOAST_ERROR_MS });
    else renderPickerError(lists, err);
  }
}

/* ------------------------------------------------------------------ */
/* Screen: server settings                                              */
/* ------------------------------------------------------------------ */

function guildSkeleton() {
  const card = () =>
    h(
      'div',
      { class: 'card skeleton-card' },
      h('div', { class: 'skeleton-card-top' }, skeletonBlock(''), skeletonBlock('skeleton-line w-50')),
      skeletonBlock('skeleton-line w-90'),
      skeletonBlock('skeleton-line w-70'),
    );
  return h(
    'div',
    { 'aria-hidden': 'true' },
    skeletonBlock('skeleton-header'),
    skeletonBlock('skeleton-toolbar'),
    h('div', { class: 'category' }, h('div', { class: 'module-grid' }, [1, 2, 3, 4, 5, 6].map(card))),
  );
}

function sidebarSkeleton() {
  return h(
    'div',
    { class: 'side-list', 'aria-hidden': 'true' },
    [1, 2, 3, 4].map(() => h('div', { class: 'side-item' }, skeletonBlock('skeleton-line w-90'))),
  );
}

function showGuildError(err, onRetry) {
  const shell = buildShell({ sidebar: false });
  mount(shell.root);
  document.title = APP_NAME;
  const retry = retryButton(onRetry);
  let card;

  if (err.status === 403) {
    card = statusCard({
      tone: 'warn',
      iconName: 'lock',
      title: "You can't manage this server",
      message: 'You need the Manage Server permission on this server to change the bot settings here.',
      actions: [backLink()],
      role: 'alert',
    });
  } else if (err.status === 404) {
    const invite = safeHref(err.body && err.body.inviteUrl);
    const absent = err.code === 'bot_not_in_guild' || Boolean(invite);
    card = statusCard({
      tone: 'accent',
      iconName: absent ? 'plus' : 'server',
      title: absent ? 'The bot is not in this server yet' : 'Server not found',
      message: absent
        ? 'Add the bot to this server first, then come back here to configure it.'
        : 'This server is not in your list, or the bot has left it.',
      actions: [
        invite ? h('a', { class: 'btn btn-primary', href: invite, target: '_blank', rel: 'noopener' }, icon('plus'), 'Add the bot') : null,
        backLink(),
      ].filter(Boolean),
      role: 'alert',
    });
  } else {
    const info = describeError(err);
    card = statusCard({
      tone: 'danger',
      iconName: 'alert',
      title: info.title,
      message: info.message,
      actions: [retry, backLink()],
      role: 'alert',
    });
  }
  shell.main.append(card);
  shell.main.focus({ preventScroll: true });
}

async function showGuild(guildId, section) {
  const token = ++routeToken;
  teardownView();
  document.title = APP_NAME;
  const shell = buildShell({ sidebar: true });
  mount(shell.root);
  window.scrollTo({ top: 0, behavior: 'instant' });
  shell.main.append(guildSkeleton());
  shell.sectionsSlot.append(sidebarSkeleton());

  const since = getStore(guildId).mark();
  const [settingsRes, catalogRes] = await Promise.allSettled([
    api(`/api/guilds/${encodeURIComponent(guildId)}/settings`),
    loadCatalog(),
  ]);
  if (token !== routeToken) return;

  const failure = settingsRes.status === 'rejected' ? settingsRes.reason : catalogRes.status === 'rejected' ? catalogRes.reason : null;
  if (failure) {
    if (handleAuthError(failure)) return;
    showGuildError(failure, () => showGuild(guildId, section));
    return;
  }

  const view = buildGuildView({ guildId, data: settingsRes.value, catalog: catalogRes.value, shell, since });
  currentView = view;
  shell.main.replaceChildren(view.main);
  document.title = `${view.guildName} · ${APP_NAME}`;
  shell.main.focus({ preventScroll: true });
  if (section) requestAnimationFrame(() => view.goTo(section));
}

/** Labels of a module's editable fields (not the quick toggles, which the card lists itself), for the overview search. */
function fieldLabels(mod) {
  const quick = new Set([mod.flag, ...(mod.subToggles || []).map((sub) => sub.field)]);
  const labels = [];
  for (const group of Array.isArray(mod.groups) ? mod.groups : []) {
    labels.push(group.title);
    for (const field of Array.isArray(group.fields) ? group.fields : []) if (!quick.has(field.field)) labels.push(field.label);
  }
  return labels.filter(Boolean).join(' ');
}

/* ---- the sidebar shared by the overview and the settings pages ---- */

const guildHref = (guildId) => `#/guild/${encodeURIComponent(guildId)}`;
const modulePageHref = (guildId, key) => `${guildHref(guildId)}/module/${encodeURIComponent(key)}`;
/** The overview, scrolled to its "Server settings" card. */
const generalHref = (guildId) => `${guildHref(guildId)}/${GENERAL_KEY}`;

/**
 * The sidebar: Overview and Server settings, then every module grouped by category (from the catalog) with a dot
 * showing whether it is on; the current page is highlighted. `active` is 'overview', 'general' or null.
 * Returns { el, paint } (paint re-reads the on/off state from the store).
 */
function buildSideNav({ guildId, model, store, currentKey = null, active = null }) {
  const page = (id, href, iconName, label) =>
    h(
      'li',
      {},
      h(
        'a',
        { class: active === id ? 'side-item is-active' : 'side-item', href, 'aria-current': active === id ? 'page' : null },
        icon(iconName),
        h('span', { class: 'side-name' }, label),
      ),
    );
  const links = [];
  const groups = model.categories.map((cat) =>
    h(
      'div',
      { class: 'side-group' },
      h('h2', { class: 'side-cat' }, cat.label),
      h(
        'ul',
        { class: 'side-list' },
        cat.modules.map((mod) => {
          const dot = h('span', { class: 'status-dot', 'aria-hidden': 'true' });
          const status = h('span', { class: 'sr-only' });
          const here = mod.key === currentKey;
          links.push({ mod, dot, status });
          return h(
            'li',
            {},
            h(
              'a',
              {
                class: here ? 'side-item side-module is-active' : 'side-item side-module',
                href: modulePageHref(guildId, mod.key),
                'aria-current': here ? 'page' : null,
              },
              dot,
              h('span', { class: 'side-name' }, mod.label, status),
            ),
          );
        }),
      ),
    ),
  );
  const paint = () => {
    for (const l of links) {
      const on = store.get(l.mod.flag, l.mod.default);
      l.dot.classList.toggle('is-on', on);
      l.status.textContent = on ? ', on' : ', off';
    }
  };
  const el = h(
    'nav',
    { class: 'side-nav', 'aria-label': 'Server' },
    h('ul', { class: 'side-list side-pages' }, page('overview', guildHref(guildId), 'grid', 'Overview'), page('general', generalHref(guildId), 'settings', 'Server settings')),
    groups,
  );
  return { el, paint };
}

/** A module's two-letter tile code: the catalog's, or the first two letters of its name. */
function moduleCode(mod) {
  if (typeof mod.code === 'string' && mod.code) return mod.code;
  const letters = Array.from(String(mod.label || mod.key || '?').replace(/[^\p{L}\p{N}]/gu, ''));
  return `${(letters[0] || '?').toUpperCase()}${(letters[1] || '').toLowerCase()}`;
}

/**
 * Builds the overview page for one guild and returns handles for the router: the header, the server settings card
 * (saved with its own Save button) and a card per module. Switches are painted from the GuildStore, so toggles never
 * re-render the page.
 */
function buildGuildView({ guildId, data, catalog, shell, since = null }) {
  const store = getStore(guildId);
  const values = store.load(data && typeof data.values === 'object' && data.values ? data.values : {}, since);
  const model = normalizeCatalog(catalog);
  const guild = (data && data.guild) || { id: guildId, name: 'Server', iconUrl: null };
  const propagation = Number(data && data.propagationSeconds) > 0 ? Number(data.propagationSeconds) : DEFAULT_PROPAGATION_S;
  const totalModules = model.categories.reduce((n, cat) => n + cat.modules.length, 0);
  const settingsUrl = `/api/guilds/${encodeURIComponent(guildId)}/settings`;
  shell.setGuild(guild);

  const switches = []; // { el, field, fallback }
  const sections = [];
  let query = '';
  let mode = 'all';
  let destroyed = false;

  const moduleOn = (mod) => store.get(mod.flag, mod.default);

  function makeSwitch(field, fallback, labelId, describedBy) {
    const el = createSwitch({ labelId, describedBy, onToggle: () => store.toggle(field, fallback) });
    switches.push({ el, field, fallback });
    return el;
  }

  /* ---- module cards ---- */

  function buildEntry(mod) {
    const id = slug(mod.key);
    const labelId = `m-${id}-label`;
    const descId = `m-${id}-desc`;
    const subText = mod.subToggles.map((sub) => [sub.label, sub.description].filter(Boolean).join(' ')).join(' ');
    const entry = {
      mod,
      status: h('span', { class: 'module-status', 'aria-hidden': 'true' }),
      ownText: [mod.label, mod.key, mod.code, mod.description, mod.configCommand, fieldLabels(mod), subText].filter(Boolean).join(' ').toLowerCase(),
    };
    const toggle = makeSwitch(mod.flag, mod.default, labelId, mod.description ? descId : null);
    entry.card = h(
      'article',
      { class: 'card module is-off', 'data-module': mod.key },
      h(
        'div',
        { class: 'module-top' },
        h('span', { class: 'module-code', 'aria-hidden': 'true' }, moduleCode(mod)),
        h('h4', { class: 'module-title', id: labelId }, mod.label),
        toggle,
      ),
      mod.description ? h('p', { class: 'module-desc', id: descId }, mod.description) : null,
      h(
        'div',
        { class: 'module-foot' },
        entry.status,
        h(
          'a',
          { class: 'module-link', href: modulePageHref(guildId, mod.key) },
          'Settings',
          h('span', { class: 'sr-only' }, ` for ${mod.label}`),
          icon('forward'),
        ),
      ),
    );
    return entry;
  }

  for (const cat of model.categories) {
    const sectionId = `cat-${slug(cat.id)}`;
    const titleId = `${sectionId}-title`;
    const grid = h('div', { class: 'module-grid' });
    const section = { cat, id: cat.id, entries: [], countEl: h('span', { class: 'category-count' }) };
    for (const mod of cat.modules) {
      const entry = buildEntry(mod);
      section.entries.push(entry);
      grid.append(entry.card);
    }
    section.el = h(
      'section',
      { class: 'category', id: sectionId, 'aria-labelledby': titleId },
      h(
        'div',
        { class: 'category-head' },
        h('h3', { class: 'category-title', id: titleId }, cat.label),
        section.countEl,
        cat.description ? h('p', { class: 'category-desc' }, cat.description) : null,
      ),
      grid,
    );
    sections.push(section);
  }

  /* ---- header ---- */

  const onCount = h('span', { class: 'on-count-num num' }, '0');
  const saveStatus = h('span', { class: 'save-status', 'data-active': 'false', 'aria-hidden': 'true' }, h('span', { class: 'spinner' }), 'Saving…');
  const header = h(
    'div',
    { class: 'page-head overview-head' },
    h(
      'div',
      { class: 'page-head-text' },
      h('p', { class: 'eyebrow' }, 'OVERVIEW'),
      h('h1', { class: 'page-title', title: guild.name }, guild.name),
    ),
    saveStatus,
    h(
      'p',
      { class: 'on-count', title: `Switches save as you flip them and reach the bot within ~${propagation} seconds.` },
      onCount,
      h('span', { class: 'on-count-text' }, ` of ${totalModules} modules on`),
    ),
  );

  /* ---- the server settings card: the general settings every module uses, saved with its own button ---- */

  const generalGroups = Array.isArray(catalog && catalog.general && catalog.general.groups) ? catalog.general.groups : [];
  const generalGroup = generalGroups.find((g) => g && g.id === GENERAL_KEY) || null;
  const gFields = generalGroup && Array.isArray(generalGroup.fields) ? generalGroup.fields.filter((f) => f && f.field && f.type) : [];
  const gBaseline = new Map(); // field -> canonical JSON of the saved value
  const gBaselineValue = new Map(); // field -> a copy of the saved value (for Reset)
  const gDirty = new Set();
  let gForm = null;
  let gReady = false;
  let gSaving = false;

  const gMsg = h('span', { class: 'panel-foot-msg' });
  const gLive = h('p', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
  const gReset = h('button', { type: 'button', class: 'btn btn-secondary', onclick: () => resetGeneral() }, 'Reset');
  const gSaveLabel = h('span', {}, 'Save changes');
  const gSave = h('button', { type: 'button', class: 'btn btn-primary', title: 'Save changes (Ctrl+S)', onclick: () => saveGeneral() }, gSaveLabel);
  const gFoot = h('div', { class: 'panel-foot', hidden: true }, gMsg, gReset, gSave);
  const gBody = h('div', { class: 'panel-body panel-grid' });

  function gSnapshot(field, value) {
    gBaseline.set(field, canonical(value));
    gBaselineValue.set(field, cloneValue(value));
    gDirty.delete(field);
  }

  function paintGeneral() {
    if (destroyed) return;
    const n = gDirty.size;
    if (n > 0 && gFoot.hidden) gLive.textContent = 'You have unsaved changes';
    gFoot.hidden = n === 0;
    gMsg.textContent = n === 1 ? '1 unsaved change' : `${n} unsaved changes`;
    gReset.setAttribute('aria-disabled', String(gSaving));
    gSave.setAttribute('aria-disabled', String(gSaving));
    gSave.setAttribute('aria-busy', String(gSaving));
    gSaveLabel.textContent = gSaving ? 'Saving…' : 'Save changes';
  }

  function onGeneralChange(field, value) {
    if (!gReady) return;
    if (canonical(value) === gBaseline.get(field)) gDirty.delete(field);
    else gDirty.add(field);
    paintGeneral();
  }

  if (gFields.length) {
    try {
      const initial = {};
      for (const f of gFields) initial[f.field] = values[f.field] !== undefined ? values[f.field] : f.default;
      gForm = createForm({
        fields: gFields,
        values: initial,
        ctx: {
          guildId,
          guild: { id: guild.id || guildId, name: guild.name, iconUrl: guild.iconUrl || null },
          user: state.user,
          resources: null,
          placeholders: Array.isArray(catalog.placeholders) ? catalog.placeholders : [],
          defaultEmbedColor: Number.isInteger(values.embedColor) ? values.embedColor : null,
        },
        idPrefix: 'f-overview-general',
        onChange: onGeneralChange,
      });
      gBody.append(gForm.el);
      const now = gForm.getValues();
      for (const f of gFields) gSnapshot(f.field, now[f.field]);
    } catch {
      gForm = null;
      gBody.append(banner('error', 'These settings could not be displayed. Reload the page to try again.'));
    }
  }
  gReady = true;

  function resetGeneral() {
    if (gSaving || !gForm) return;
    const restore = {};
    for (const f of gFields) restore[f.field] = cloneValue(gBaselineValue.get(f.field));
    try {
      gForm.setValues({ ...gForm.getValues(), ...restore });
      gForm.setErrors([]);
    } catch { /* a control that cannot reset keeps its value */ }
    const now = gForm.getValues();
    for (const f of gFields) onGeneralChange(f.field, now[f.field]);
    gLive.textContent = 'Changes discarded';
  }

  async function saveGeneral() {
    if (gSaving || destroyed || !gForm || gDirty.size === 0) return;
    let problems = [];
    try { problems = gForm.validate([...gDirty]); } catch { /* saved as it is */ }
    if (problems.length) {
      try { gForm.focusPath(problems[0].path); } catch { generalCard.scrollIntoView({ block: 'center' }); }
      toast(problems[0].message, { kind: 'error', key: 'save-error', ms: TOAST_ERROR_MS });
      return;
    }
    gSaving = true;
    paintGeneral();
    const changes = {};
    for (const field of gDirty) changes[field] = gForm.getValue(field);
    try {
      const res = await api(settingsUrl, { method: 'PATCH', body: { changes } });
      if (destroyed) return;
      const saved = res && typeof res.values === 'object' && res.values ? res.values : {};
      const patch = {};
      for (const [field, sent] of Object.entries(changes)) {
        if (saved[field] !== undefined && canonical(gForm.getValue(field)) === canonical(sent)) patch[field] = saved[field];
      }
      try { gForm.setValues({ ...gForm.getValues(), ...patch }); } catch { /* keep what the user sees */ }
      gForm.setErrors([]);
      for (const field of Object.keys(changes)) {
        const value = gForm.getValue(field);
        if (canonical(value) === canonical(patch[field] ?? changes[field])) gSnapshot(field, value);
      }
      toast('Saved', { kind: 'success', key: 'saved' });
      showNotices(res);
      gLive.textContent = 'Changes saved';
    } catch (err) {
      if (!destroyed && err && err.status === 400) {
        const path = err.body && typeof err.body.path === 'string' ? err.body.path : '';
        const message = err.serverMessage || 'One of the values is not valid.';
        try {
          gForm.setErrors([{ path, message }]);
          gForm.focusPath(path);
        } catch { /* the toast below still tells the user */ }
        toast(message, { kind: 'error', key: 'save-error', ms: TOAST_ERROR_MS });
      } else {
        reportSaveError(err);
      }
    } finally {
      gSaving = false;
      paintGeneral();
    }
  }

  const generalCard = gFields.length
    ? h(
      'section',
      { class: 'card panel general-card', id: 'overview-general', 'aria-labelledby': 'overview-general-title' },
      h(
        'div',
        { class: 'panel-head' },
        h('h2', { class: 'panel-title', id: 'overview-general-title' }, generalGroup.title || 'Server settings'),
        h('p', { class: 'panel-desc' }, 'Settings every module uses.'),
        generalGroups.length > 1
          ? h('a', { class: 'panel-link', href: modulePageHref(guildId, GENERAL_KEY) }, 'More server settings', icon('forward'))
          : null,
      ),
      gBody,
      gFoot,
      gLive,
    )
    : null;

  /* ---- modules toolbar ---- */

  const search = h('input', {
    type: 'search',
    class: 'search-input',
    placeholder: 'Search modules',
    'aria-label': 'Search modules and options',
    autocomplete: 'off',
    spellcheck: 'false',
    oninput: () => {
      query = search.value;
      applyFilters();
    },
    onkeydown: (event) => {
      if (event.key === 'Escape' && search.value) {
        event.preventDefault();
        search.value = '';
        query = '';
        applyFilters();
      }
    },
  });

  const segButtons = {};
  const segmented = h(
    'div',
    { class: 'segmented', role: 'group', 'aria-label': 'Show modules' },
    [['all', 'All'], ['on', 'On'], ['off', 'Off']].map(([id, label]) => {
      segButtons[id] = h(
        'button',
        {
          type: 'button',
          class: 'seg-btn',
          'aria-pressed': id === mode ? 'true' : 'false',
          onclick: () => {
            mode = id;
            for (const [other, btn] of Object.entries(segButtons)) btn.setAttribute('aria-pressed', other === mode ? 'true' : 'false');
            applyFilters();
          },
        },
        label,
      );
      return segButtons[id];
    }),
  );

  const resultNote = h('p', { class: 'result-note', role: 'status', 'aria-live': 'polite' });

  function clearFilters() {
    query = '';
    mode = 'all';
    search.value = '';
    for (const [id, btn] of Object.entries(segButtons)) btn.setAttribute('aria-pressed', id === 'all' ? 'true' : 'false');
    applyFilters();
  }

  const emptyState = h(
    'div',
    { class: 'empty-state', hidden: true },
    h('p', {}, 'No modules match. Try another name or show all modules.'),
    h('button', { type: 'button', class: 'btn btn-secondary', onclick: clearFilters }, 'Show all modules'),
  );

  const modulesSection = h(
    'section',
    { class: 'modules', 'aria-labelledby': 'modules-title' },
    h(
      'div',
      { class: 'modules-head' },
      h(
        'div',
        { class: 'page-head-text' },
        h('h2', { class: 'section-title', id: 'modules-title' }, 'Modules'),
        h(
          'p',
          { class: 'page-sub' },
          "Turn a module on here or with /module in Discord. Its settings stay saved while it's off.",
        ),
      ),
      h('div', { class: 'modules-tools', role: 'search' }, segmented, h('div', { class: 'search' }, icon('search'), search)),
    ),
    resultNote,
    emptyState,
    sections.map((s) => s.el),
  );

  /* ---- filtering ---- */

  function applyFilters() {
    const tokens = query.toLowerCase().split(/\s+/).filter(Boolean);
    let shown = 0;
    for (const section of sections) {
      let sectionShown = 0;
      for (const entry of section.entries) {
        let visible = tokens.every((t) => entry.ownText.includes(t));
        if (visible && mode !== 'all') visible = moduleOn(entry.mod) === (mode === 'on');
        entry.card.hidden = !visible;
        if (visible) sectionShown++;
      }
      section.el.hidden = sectionShown === 0;
      section.countEl.textContent = pluralize(sectionShown, 'module');
      shown += sectionShown;
    }
    emptyState.hidden = shown !== 0;
    const filtering = tokens.length > 0 || mode !== 'all';
    resultNote.textContent = filtering ? `Showing ${shown} of ${totalModules} modules` : '';
  }

  /* ---- painting from the store, sidebar ---- */

  const sideNav = buildSideNav({ guildId, model, store, active: 'overview' });
  shell.sectionsSlot.replaceChildren(sideNav.el);

  function paint() {
    for (const s of switches) paintSwitch(s.el, store.get(s.field, s.fallback), store.isPending(s.field));
    let onTotal = 0;
    for (const section of sections) {
      for (const entry of section.entries) {
        const isOn = moduleOn(entry.mod);
        if (isOn) onTotal++;
        entry.card.classList.toggle('is-on', isOn);
        entry.card.classList.toggle('is-off', !isOn);
        entry.status.textContent = isOn ? 'ON' : 'OFF';
      }
    }
    onCount.textContent = String(onTotal);
    saveStatus.setAttribute('data-active', String(store.busy));
    sideNav.paint();
  }

  function goTo(sectionId) {
    if (sectionId === GENERAL_KEY && generalCard) {
      generalCard.scrollIntoView({ block: 'start' });
      return;
    }
    const section = sections.find((s) => s.id === sectionId);
    if (!sectionId || !section) {
      window.scrollTo({ top: 0 });
      return;
    }
    if (!section.el.hidden) section.el.scrollIntoView({ block: 'start' });
  }

  const main = h('div', { class: 'guild-view' }, header, generalCard, modulesSection);
  const unsubscribe = store.subscribe(paint);
  paint();
  applyFilters();
  paintGeneral();
  activeSearch = search;

  return {
    kind: 'overview',
    main,
    guildName: guild.name,
    goTo,
    isDirty: () => gDirty.size > 0,
    save: saveGeneral,
    destroy() {
      destroyed = true;
      unsubscribe();
      if (activeSearch === search) activeSearch = null;
      try { if (gForm) gForm.destroy(); } catch { /* already gone */ }
    },
  };
}

/* ------------------------------------------------------------------ */
/* Screens: module and general settings pages                           */
/* ------------------------------------------------------------------ */

const GENERAL_KEY = 'general';
const DEFAULT_PROPAGATION_S = 30;
const LEAVE_PROMPT = 'You have unsaved changes. Leave this page and discard them?';

function settingsSkeleton() {
  const card = () =>
    h(
      'div',
      { class: 'card skeleton-card' },
      skeletonBlock('skeleton-line w-30'),
      skeletonBlock('skeleton-line w-90'),
      skeletonBlock('skeleton-line w-70'),
      skeletonBlock('skeleton-line w-50'),
    );
  return h('div', { class: 'module-page', 'aria-hidden': 'true' }, skeletonBlock('skeleton-crumb'), skeletonBlock('skeleton-header'), card(), card());
}

const isResources = (value) =>
  Boolean(value) && typeof value === 'object' && Array.isArray(value.channels) && Array.isArray(value.roles);

/** The page a `#/guild/<id>/module/<key>` route shows, or null for an unknown module. */
function resolvePage(catalog, model, key) {
  const usable = (groups, hidden = []) =>
    (Array.isArray(groups) ? groups : [])
      .map((group) => ({
        ...group,
        fields: (Array.isArray(group && group.fields) ? group.fields : []).filter((f) => f && f.field && f.type && !hidden.includes(f.field)),
      }))
      .filter((group) => group.fields.length > 0);

  if (key === GENERAL_KEY) {
    const general = (catalog && catalog.general) || {};
    return {
      key,
      mod: null,
      label: 'Server settings',
      code: null,
      num: null,
      category: null,
      description: general.description || 'Time zone, embed colour and command prefix for this server.',
      groups: usable(general.groups),
      collections: [],
    };
  }
  const all = model.categories.flatMap((cat) => cat.modules.map((m) => ({ mod: m, category: cat })));
  const index = all.findIndex((x) => x.mod.key === key);
  if (index < 0) return null;
  const { mod, category } = all[index];
  const collections = (Array.isArray(mod.collections) ? mod.collections : []).filter((k) => catalog.collections && catalog.collections[k]);
  return {
    key,
    mod,
    label: mod.label,
    code: moduleCode(mod),
    num: String(index + 1).padStart(2, '0'),
    category: category.label,
    description: mod.description || '',
    groups: usable(mod.groups, [mod.flag]),
    collections,
  };
}

const cloneValue = (value) => {
  try { return structuredClone(value); } catch { return JSON.parse(JSON.stringify(value ?? null)); }
};

async function showModule(guildId, moduleKey) {
  const token = ++routeToken;
  teardownView();
  document.title = APP_NAME;
  const shell = buildShell({ sidebar: true });
  mount(shell.root);
  window.scrollTo({ top: 0, behavior: 'instant' });
  shell.main.append(settingsSkeleton());
  shell.sectionsSlot.append(sidebarSkeleton());

  const base = `/api/guilds/${encodeURIComponent(guildId)}`;
  const since = getStore(guildId).mark();
  const [settingsRes, catalogRes, resourcesRes] = await Promise.allSettled([api(`${base}/settings`), loadCatalog(), api(`${base}/resources`)]);
  if (token !== routeToken) return;

  const failure = settingsRes.status === 'rejected' ? settingsRes.reason : catalogRes.status === 'rejected' ? catalogRes.reason : null;
  if (failure) {
    if (handleAuthError(failure)) return;
    showGuildError(failure, () => showModule(guildId, moduleKey));
    return;
  }
  if (resourcesRes.status === 'rejected' && handleAuthError(resourcesRes.reason)) return;

  const catalog = catalogRes.value;
  const model = normalizeCatalog(catalog);
  const page = resolvePage(catalog, model, moduleKey);
  if (!page) {
    shell.main.replaceChildren(
      statusCard({
        tone: 'accent',
        iconName: 'server',
        title: 'Module not found',
        message: 'This module does not exist. It may have been renamed.',
        actions: [h('a', { class: 'btn btn-secondary', href: guildHref(guildId) }, icon('back'), 'Back to overview')],
        role: 'alert',
      }),
    );
    shell.main.focus({ preventScroll: true });
    shell.setGuild((settingsRes.value && settingsRes.value.guild) || { id: guildId, name: 'Server', iconUrl: null });
    const nav = buildSideNav({ guildId, model, store: getStore(guildId) });
    shell.sectionsSlot.replaceChildren(nav.el);
    nav.paint();
    return;
  }

  const resources = resourcesRes.status === 'fulfilled' && isResources(resourcesRes.value) ? resourcesRes.value : null;
  const view = buildSettingsView({ guildId, data: settingsRes.value, catalog, model, page, resources, shell, since });
  currentView = view;
  shell.main.replaceChildren(view.main);
  document.title = `${page.label} · ${view.guildName} · ${APP_NAME}`;
  shell.main.focus({ preventScroll: true });
  view.revealNav();
}

/**
 * Builds one module's (or the general) settings page: header with the on/off switch, one form per group, one
 * section per collection, and the unsaved-changes bar. Boolean flags are painted from the GuildStore, so the
 * switch here and the overview stay in sync; everything else is saved explicitly with ONE PATCH.
 */
function buildSettingsView({ guildId, data, catalog, model, page, resources, shell, since = null }) {
  const store = getStore(guildId);
  // The values the forms start from: the GET's, except a field a save has answered since the request was sent.
  const values = store.load(data && typeof data.values === 'object' && data.values ? data.values : {}, since);
  const guild = (data && data.guild) || { id: guildId, name: 'Server', iconUrl: null };
  const propagation = Number(data && data.propagationSeconds) > 0 ? Number(data.propagationSeconds) : DEFAULT_PROPAGATION_S;
  const mod = page.mod;
  const idBase = `mp-${slug(page.key)}`;
  const settingsUrl = `/api/guilds/${encodeURIComponent(guildId)}/settings`;
  let destroyed = false;
  let saving = false;
  let ready = false; // false while the forms are being built: their first onChange calls are not edits
  let lastField = null; // where focus returns when the bar closes under the keyboard

  const ctx = {
    guildId,
    guild: { id: guild.id || guildId, name: guild.name, iconUrl: guild.iconUrl || null },
    user: state.user,
    resources,
    placeholders: Array.isArray(catalog.placeholders) ? catalog.placeholders : [],
    // Previews of embeds without their own colour use the server's embed colour, like the bot does.
    defaultEmbedColor: Number.isInteger(values.embedColor) ? values.embedColor : null,
    reloadResources: () => reloadResources(),
  };

  /* ---- resources (channels, roles, emojis) ---- */

  const entries = []; // one per group: { group, form, card }
  let resourcesPromise = null;

  function reloadResources() {
    if (resourcesPromise) return resourcesPromise;
    // Always an explicit reload (the Refresh and Retry buttons): skip the server's 30 s cache, or a channel the admin
    // just created would not show up.
    const promise = api(`/api/guilds/${encodeURIComponent(guildId)}/resources?fresh=1`)
      .then((res) => {
        if (!isResources(res)) throw new ApiError(502, 'bad_response', '', null);
        ctx.resources = res;
        if (!destroyed) {
          for (const entry of entries) {
            try { entry.form.setResources(res); } catch { /* one broken control must not stop the others */ }
          }
          resourcesWarning.hidden = true;
        }
        return res;
      })
      .catch((err) => {
        // Every caller is a button the admin pressed; the pickers swallow the rejection, so say what went wrong here.
        if (!destroyed && !handleAuthError(err)) {
          toast(describeError(err).message, { kind: 'error', key: 'resources-error', ms: TOAST_ERROR_MS });
        }
        throw err;
      })
      .finally(() => {
        if (resourcesPromise === promise) resourcesPromise = null;
      });
    resourcesPromise = promise;
    return promise;
  }

  const retryResources = h('button', { type: 'button', class: 'btn btn-secondary btn-sm' }, icon('refresh'), 'Retry');
  retryResources.addEventListener('click', () => {
    retryResources.disabled = true;
    reloadResources()
      .catch(() => { /* reloadResources already told the admin */ })
      .finally(() => { retryResources.disabled = false; });
  });
  const resourcesWarning = banner('warn', "Couldn't load this server's channels, roles and emojis. You can still edit other settings; pickers may be limited.", [retryResources]);
  resourcesWarning.hidden = resources !== null;

  /* ---- dirty tracking ---- */

  const owner = new Map(); // top-level field -> entry
  const baseline = new Map(); // field -> canonical JSON of the saved value
  const baselineValue = new Map(); // field -> a copy of the saved value (for Reset)
  const current = new Map(); // field -> canonical JSON of the value in the form
  const dirty = new Set();

  function snapshot(entry, field, value) {
    const text = canonical(value);
    baseline.set(field, text);
    baselineValue.set(field, cloneValue(value));
    current.set(field, text);
    dirty.delete(field);
    owner.set(field, entry);
  }

  function markDirty(field) {
    if (current.get(field) !== baseline.get(field)) dirty.add(field);
    else dirty.delete(field);
  }

  function onFieldChange(field, value) {
    if (!ready) return;
    current.set(field, canonical(value));
    markDirty(field);
    paintBar();
  }

  /* ---- header ---- */

  const titleId = `${idBase}-title`;
  const descId = `${idBase}-desc`;
  const saveStatus = h('span', { class: 'save-status', 'data-active': 'false', 'aria-hidden': 'true' }, h('span', { class: 'spinner' }), 'Saving…');
  const stateText = h('span', { class: 'mh-state', 'aria-hidden': 'true' });
  const toggle = mod ? createSwitch({ labelId: titleId, describedBy: mod.description ? descId : null, onToggle: () => store.toggle(mod.flag, mod.default) }) : null;
  const offNotice = mod
    ? h('p', { class: 'notice-warn', role: 'status', hidden: true }, `${page.label} is off. The bot ignores these settings until you turn it on; they stay saved.`)
    : null;

  const header = h(
    'section',
    { class: 'module-header', 'aria-labelledby': titleId },
    h(
      'div',
      { class: 'mh-tile', 'aria-hidden': 'true' },
      page.code ? h('span', { class: 'mh-code' }, page.code) : icon('settings'),
      page.num ? h('span', { class: 'mh-num' }, page.num) : null,
    ),
    h(
      'div',
      { class: 'mh-text' },
      h('h1', { class: 'mh-title', id: titleId }, page.label),
      page.description ? h('p', { class: 'mh-desc', id: descId }, page.description) : null,
    ),
    mod ? h('div', { class: 'mh-switch' }, stateText, toggle) : null,
    h(
      'div',
      { class: 'mh-meta' },
      h('p', { class: 'mh-note' }, `Changes reach the bot within ~${propagation} seconds.`),
      mod && mod.configCommand ? h('p', { class: 'mh-note' }, 'Also in Discord: ', h('code', {}, mod.configCommand)) : null,
      saveStatus,
    ),
  );

  const crumbs = h(
    'nav',
    { class: 'crumbs', 'aria-label': 'Breadcrumb' },
    h(
      'ol',
      { class: 'crumb-list' },
      h('li', {}, h('a', { class: 'crumb-link', href: guildHref(guildId) }, 'Overview')),
      page.category ? h('li', { class: 'crumb-sep', 'aria-hidden': 'true' }, '/') : null,
      page.category ? h('li', {}, page.category) : null,
      h('li', { class: 'crumb-sep', 'aria-hidden': 'true' }, '/'),
      h('li', { class: 'crumb-here', 'aria-current': 'page' }, page.label),
    ),
  );

  /* ---- forms: one card per group ---- */

  const sections = []; // { id, label, el } for the "On this page" chips
  const groupCards = [];
  for (const group of page.groups) {
    const gTitleId = `${idBase}-${slug(group.id)}-title`;
    const entry = { group, form: null, card: null };
    const body = h('div', { class: 'group-body' });
    try {
      const initial = {};
      for (const f of group.fields) initial[f.field] = values[f.field] !== undefined ? values[f.field] : f.default;
      entry.form = createForm({
        fields: group.fields,
        values: initial,
        ctx,
        idPrefix: `f-${slug(page.key)}-${slug(group.id)}`,
        onChange: onFieldChange,
      });
      body.append(entry.form.el);
      const initialValues = entry.form.getValues();
      for (const f of group.fields) snapshot(entry, f.field, initialValues[f.field]);
      entries.push(entry);
    } catch {
      body.append(banner('error', 'This group of settings could not be displayed. Reload the page to try again.'));
    }
    entry.card = h(
      'section',
      { class: 'card group-card', id: `${idBase}-${slug(group.id)}`, 'aria-labelledby': gTitleId },
      h(
        'div',
        { class: 'group-head' },
        h('h2', { class: 'group-title', id: gTitleId }, group.title || 'Settings'),
        group.description ? h('p', { class: 'group-desc' }, group.description) : null,
      ),
      body,
    );
    groupCards.push(entry.card);
    sections.push({ label: group.title || 'Settings', el: entry.card });
  }

  /* ---- collections ---- */

  const handles = [];
  const collectionSections = [];
  for (const key of page.collections) {
    const collection = catalog.collections[key];
    const cTitleId = `${idBase}-c-${slug(key)}-title`;
    const host = h('div', { class: 'collection-host' });
    try {
      const handle = mountCollection(host, { collection, ctx });
      if (handle && typeof handle.destroy === 'function') handles.push(handle);
    } catch {
      host.append(banner('error', 'This list could not be displayed. Reload the page to try again.'));
    }
    const el = h(
      'section',
      { class: 'collection-section', id: `${idBase}-c-${slug(key)}`, 'aria-labelledby': cTitleId },
      h(
        'div',
        { class: 'section-head' },
        h('h2', { class: 'section-title', id: cTitleId }, collection.label || key),
        collection.description ? h('p', { class: 'section-desc' }, collection.description) : null,
      ),
      host,
    );
    collectionSections.push(el);
    sections.push({ label: collection.label || key, el });
  }

  const jump =
    sections.length >= 3
      ? h(
        'nav',
        { class: 'jump', 'aria-label': 'On this page' },
        sections.map((s) => h('button', { type: 'button', class: 'jump-chip', onclick: () => s.el.scrollIntoView({ block: 'start' }) }, s.label)),
      )
      : null;

  const emptyNote =
    sections.length === 0
      ? h(
        'div',
        { class: 'card empty-card' },
        h('p', { class: 'empty-title' }, mod ? 'Nothing else to configure' : 'No general settings available'),
        h('p', { class: 'empty-text' }, mod ? 'This module has no extra options. Use the switch above to turn it on or off.' : 'Try reloading the page.'),
      )
      : null;

  /* ---- unsaved-changes bar ---- */

  const live = h('p', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
  const countText = h('span', { class: 'save-bar-msg' });
  const resetBtn = h('button', { type: 'button', class: 'btn btn-secondary', onclick: () => reset() }, 'Reset');
  const saveLabel = h('span', {}, 'Save changes');
  const saveSpinner = h('span', { class: 'spinner on-accent', hidden: true });
  const saveBtn = h(
    'button',
    { type: 'button', class: 'btn btn-primary save-btn', title: 'Save changes (Ctrl+S)', onclick: () => save() },
    saveSpinner,
    saveLabel,
  );
  const bar = h(
    'div',
    { class: 'save-bar', role: 'region', 'aria-label': 'Unsaved changes', hidden: true },
    countText,
    h('div', { class: 'save-bar-actions' }, resetBtn, saveBtn),
  );

  const main = h(
    'div',
    { class: 'module-page' },
    crumbs,
    header,
    offNotice,
    resourcesWarning,
    jump,
    groupCards,
    collectionSections,
    emptyNote,
    live,
    bar,
  );
  main.addEventListener('focusin', (event) => {
    if (!bar.contains(event.target)) lastField = event.target;
  });
  ready = true;

  function paintBar() {
    if (destroyed) return;
    const any = dirty.size > 0;
    if (any && bar.hidden) live.textContent = 'You have unsaved changes';
    bar.hidden = !any;
    main.classList.toggle('has-bar', any);
    document.body.classList.toggle('has-savebar', any);
    countText.textContent = any ? (dirty.size === 1 ? '1 unsaved change' : `${dirty.size} unsaved changes`) : '';
    // aria-disabled (not disabled) so the focused button keeps focus while the request runs.
    resetBtn.setAttribute('aria-disabled', String(saving));
    saveBtn.setAttribute('aria-disabled', String(saving));
    saveBtn.setAttribute('aria-busy', String(saving));
    saveSpinner.hidden = !saving;
    saveLabel.textContent = saving ? 'Saving…' : 'Save changes';
  }

  /** When the bar closes with focus inside it, hand focus back to the last field the user touched. */
  function closeBar() {
    const hadFocus = bar.contains(document.activeElement);
    paintBar();
    if (hadFocus && bar.hidden && lastField && lastField.isConnected) lastField.focus({ preventScroll: true });
  }

  function reset() {
    if (saving) return;
    for (const entry of entries) {
      const restore = {};
      for (const f of entry.group.fields) restore[f.field] = cloneValue(baselineValue.get(f.field));
      try {
        entry.form.setValues({ ...entry.form.getValues(), ...restore });
        entry.form.setErrors([]);
      } catch { /* a control that cannot reset keeps its value; the bar below stays honest */ }
      const now = entry.form.getValues();
      for (const f of entry.group.fields) {
        current.set(f.field, canonical(now[f.field]));
        markDirty(f.field);
      }
    }
    closeBar();
    live.textContent = 'Changes discarded';
  }

  function applySaved(res, sentValues) {
    const saved = res && typeof res.values === 'object' && res.values ? res.values : null;

    // Keep the overview switches (and this page's own) in step with what the server stored.
    const flags = {};
    for (const [field, sent] of sentValues) {
      const value = saved && saved[field] !== undefined ? saved[field] : sent;
      if (typeof value === 'boolean') flags[field] = value;
    }
    store.confirm(flags);
    if (destroyed) return;

    for (const entry of entries) {
      const fields = entry.group.fields.map((f) => f.field).filter((f) => sentValues.has(f));
      if (!fields.length) continue;
      // Controls get the server's normalised value, unless the user already edited it again.
      const patch = {};
      for (const f of fields) {
        const now = canonical(entry.form.getValue(f));
        if (saved && saved[f] !== undefined && now === canonical(sentValues.get(f)) && canonical(saved[f]) !== now) patch[f] = saved[f];
      }
      if (Object.keys(patch).length) {
        try { entry.form.setValues({ ...entry.form.getValues(), ...patch }); } catch { /* keep what the user sees */ }
      }
      entry.form.setErrors([]);
      for (const f of fields) {
        const value = entry.form.getValue(f);
        if (canonical(value) === canonical(sentValues.get(f)) || patch[f] !== undefined) {
          snapshot(entry, f, value);
        } else {
          // Edited again while the request was in flight: what was sent is saved, the newer edit is still pending.
          baseline.set(f, canonical(sentValues.get(f)));
          baselineValue.set(f, cloneValue(sentValues.get(f)));
          current.set(f, canonical(value));
          markDirty(f);
        }
      }
    }
  }

  function showFieldError(err) {
    const path = err.body && typeof err.body.path === 'string' ? err.body.path : '';
    const message = err.serverMessage || 'One of the values is not valid.';
    const entry = path ? owner.get(path.split('.')[0]) : null;
    for (const e of entries) {
      try { e.form.setErrors(e === entry ? [{ path, message }] : []); } catch { /* the toast below still tells the user */ }
    }
    if (entry) {
      // focusPath also scrolls the field into view; the card is the fallback.
      try { entry.form.focusPath(path); } catch { entry.card.scrollIntoView({ block: 'center' }); }
    }
    toast(message, { kind: 'error', key: 'save-error', ms: TOAST_ERROR_MS });
  }

  /**
   * A number box that was emptied (or typed out of range) still shows its error but holds the last valid number, which
   * would be sent as if nothing were wrong; Ctrl+S gets here before the box loses focus. Stops the save on the first one,
   * with the field's own message, as a field the server refused.
   */
  function blockedByField() {
    let first = null;
    for (const entry of entries) {
      let problems = [];
      try { problems = entry.form.validate([...dirty]); } catch { /* a control that cannot be checked is saved as it is */ }
      if (problems.length && !first) first = { entry, problem: problems[0] };
    }
    if (!first) return false;
    try { first.entry.form.focusPath(first.problem.path); } catch { first.entry.card.scrollIntoView({ block: 'center' }); }
    toast(first.problem.message, { kind: 'error', key: 'save-error', ms: TOAST_ERROR_MS });
    return true;
  }

  async function save() {
    if (saving || dirty.size === 0 || destroyed) return;
    if (blockedByField()) return;
    saving = true;
    paintBar();
    const changes = {};
    const sentValues = new Map();
    for (const field of dirty) {
      const value = owner.get(field).form.getValue(field);
      changes[field] = value;
      sentValues.set(field, cloneValue(value));
    }
    try {
      const res = await api(settingsUrl, { method: 'PATCH', body: { changes } });
      applySaved(res, sentValues);
      toast('Saved', { kind: 'success', key: 'saved' });
      showNotices(res);
      if (!destroyed) live.textContent = 'Changes saved';
    } catch (err) {
      if (destroyed) {
        reportSaveError(err);
      } else if (err && err.status === 400) {
        showFieldError(err);
      } else {
        reportSaveError(err);
      }
    } finally {
      saving = false;
      if (!destroyed) closeBar();
    }
  }

  /* ---- painting from the store, sidebar ---- */

  const moduleNav = buildSideNav({ guildId, model, store, currentKey: mod ? mod.key : null, active: mod ? null : GENERAL_KEY });

  function paint() {
    if (mod) {
      const on = store.get(mod.flag, mod.default);
      paintSwitch(toggle, on, store.isPending(mod.flag));
      stateText.textContent = on ? 'Module on' : 'Module off';
      header.classList.toggle('is-on', on);
      offNotice.hidden = on;
    }
    saveStatus.setAttribute('data-active', String(store.busy));
    moduleNav.paint();
  }

  shell.setGuild(guild);
  shell.sectionsSlot.replaceChildren(moduleNav.el);

  const unsubscribe = store.subscribe(paint);
  paint();
  paintBar();

  return {
    kind: 'settings',
    main,
    guildName: guild.name,
    // An open record editor (a dialog over the page) counts too: leaving the page would throw its edits away.
    isDirty: () => dirty.size > 0 || handles.some((handle) => typeof handle.isDirty === 'function' && handle.isDirty()),
    save,
    // Ctrl+S while a list's dialog is open saves that dialog (when it can) and never the page behind it.
    dialogOpen: () => handles.some((handle) => typeof handle.dialogOpen === 'function' && handle.dialogOpen()),
    saveDialog() {
      for (const handle of handles) if (typeof handle.saveEditor === 'function' && handle.saveEditor()) return;
    },
    /** Scrolls the sidebar so the current module is visible. */
    revealNav() {
      requestAnimationFrame(() => {
        const here = shell.sectionsSlot.querySelector('.side-module[aria-current="page"]');
        const aside = here && here.closest('.sidebar');
        if (!aside) return;
        // Only move the list when the current module is out of sight.
        const top = here.offsetTop;
        if (top < aside.scrollTop || top + here.offsetHeight > aside.scrollTop + aside.clientHeight) {
          aside.scrollTop = Math.max(0, top - aside.clientHeight / 3);
        }
      });
    },
    destroy() {
      destroyed = true;
      unsubscribe();
      document.body.classList.remove('has-savebar');
      for (const handle of handles) {
        try { handle.destroy(); } catch { /* already gone */ }
      }
      for (const entry of entries) {
        try { entry.form.destroy(); } catch { /* already gone */ }
      }
    },
  };
}

/* ------------------------------------------------------------------ */
/* Router                                                               */
/* ------------------------------------------------------------------ */

/**
 * `#/guild/<id>` (overview), `#/guild/<id>/<category>` (overview scrolled to a category),
 * `#/guild/<id>/module/<key>` (a module's settings; key `general` is the general settings page).
 * Anything else is the picker.
 */
function parseRoute(hash) {
  const page = /^#\/guild\/(\d{1,30})\/module\/([A-Za-z0-9_-]{1,40})\/?$/.exec(hash || '');
  if (page) return { name: 'module', id: page[1], section: null, module: page[2] };
  const match = /^#\/guild\/(\d{1,30})(?:\/([A-Za-z0-9_-]{1,40}))?\/?$/.exec(hash || '');
  if (match) return { name: 'guild', id: match[1], section: match[2] || null };
  return { name: 'picker', id: null, section: null };
}

/** True when the settings page on screen has edits that were not saved. */
function hasUnsavedChanges() {
  return Boolean(currentView && typeof currentView.isDirty === 'function' && currentView.isDirty());
}

/** Unsaved edits on the page, or module switches still on their way to the server. */
function hasUnsavedWork() {
  if (hasUnsavedChanges()) return true;
  for (const store of stores.values()) if (store.busy) return true;
  return false;
}

function route() {
  if (!state.user) return;
  acceptedHash = location.hash;
  const next = parseRoute(location.hash);
  if (next.name === 'guild' && currentRoute && currentRoute.name === 'guild' && currentRoute.id === next.id && currentView && currentView.goTo) {
    currentView.goTo(next.section); // same server: only the section anchor changed
    return;
  }
  currentRoute = next;
  if (next.name === 'module') showModule(next.id, next.module);
  else if (next.name === 'guild') showGuild(next.id, next.section);
  else showPicker();
}

function onHashChange() {
  if (location.hash === acceptedHash) return; // the echo of a cancelled navigation
  // Moving between sections of the overview on screen keeps its edits, so it needs no question.
  const next = parseRoute(location.hash);
  const sameOverview = next.name === 'guild' && currentRoute && currentRoute.name === 'guild' && currentRoute.id === next.id;
  if (!sameOverview && hasUnsavedChanges() && !window.confirm(LEAVE_PROMPT)) {
    // Put the old address back without a history entry or another hashchange.
    history.replaceState(null, '', location.pathname + location.search + acceptedHash);
    return;
  }
  route();
}

async function boot() {
  renderLoading();
  try {
    const me = await api('/api/me');
    state.user = me && me.user ? me.user : null;
  } catch (err) {
    if (err.status === 401) {
      renderLoggedOut({ error: state.loginError });
    } else {
      renderBootError(err);
    }
    return;
  }
  if (!state.user) {
    renderLoggedOut({ error: state.loginError });
    return;
  }
  state.loginError = false;
  route();
}

/**
 * Ctrl/Cmd+S on a settings page. While a dialog is open (a record editor over the page) the page behind it is not what
 * the user is looking at: the dialog's own Save runs when it has one, otherwise nothing is saved. The browser's "save
 * page" dialog is suppressed either way.
 * @param {{ key?: string, ctrlKey?: boolean, metaKey?: boolean, altKey?: boolean, shiftKey?: boolean, preventDefault: () => void }} event
 * @param {{ save?: () => unknown, dialogOpen?: () => boolean, saveDialog?: () => unknown }|null} view
 * @returns {boolean} whether the key was taken
 */
function handleSaveShortcut(event, view) {
  if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey || String(event.key).toLowerCase() !== 's') return false;
  if (!view || typeof view.save !== 'function') return false;
  event.preventDefault();
  if (typeof view.dialogOpen === 'function' && view.dialogOpen()) {
    if (typeof view.saveDialog === 'function') view.saveDialog();
    return true;
  }
  view.save();
  return true;
}

function start() {
  state.loginError = new URLSearchParams(location.search).get('error') === 'login_failed';
  if (location.search) history.replaceState(null, '', location.pathname + location.hash);

  window.addEventListener('hashchange', onHashChange);

  // "/" focuses the search box, unless the user is already typing somewhere.
  document.addEventListener('keydown', (event) => {
    if (event.key !== '/' || event.ctrlKey || event.metaKey || event.altKey || !activeSearch) return;
    const target = event.target;
    if (target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) return;
    event.preventDefault();
    activeSearch.focus();
    activeSearch.select();
  });

  // Ctrl/Cmd+S saves the settings page instead of the browser's "save page" dialog.
  document.addEventListener('keydown', (event) => handleSaveShortcut(event, currentView));

  window.addEventListener('beforeunload', (event) => {
    if (!leaving && hasUnsavedWork()) {
      event.preventDefault();
      event.returnValue = '';
    }
  });

  if (typeof matchMedia === 'function') {
    matchMedia('(min-width: 901px)').addEventListener('change', (event) => {
      if (event.matches && activeShell) activeShell.setDrawer(false);
    });
  }

  boot();
}

export { parseRoute, normalizeCatalog, initialsOf, GuildStore, handleSaveShortcut };

if (typeof document !== 'undefined') start();
