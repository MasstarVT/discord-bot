// Bot Dashboard: a dependency-free single-page UI (vanilla ES module, no build step).
//
// Layout of this file (shared DOM helpers, the API client, toasts and small components live in lib.js;
// field controls in forms.js, record lists and the embed editor in collections.js / embed-editor.js):
//   constants, state, theme, the per-guild save queue, shell, screens (picker, overview, module and general
//   settings pages with the unsaved-changes bar), router, start().
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

const APP_NAME = 'Bot Dashboard';
const THEME_KEY = 'bot-dashboard-theme';
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

/* ------------------------------------------------------------------ */
/* Theme                                                                */
/* ------------------------------------------------------------------ */

function readStoredTheme() {
  try {
    const value = localStorage.getItem(THEME_KEY);
    return value === 'light' || value === 'dark' ? value : null;
  } catch {
    return null;
  }
}

function storeTheme(theme) {
  try { localStorage.setItem(THEME_KEY, theme); } catch { /* private mode: the choice just will not persist */ }
}

function applyTheme(theme) {
  if (theme) document.documentElement.setAttribute('data-theme', theme);
  else document.documentElement.removeAttribute('data-theme');
}

function currentTheme() {
  const explicit = document.documentElement.getAttribute('data-theme');
  if (explicit === 'light' || explicit === 'dark') return explicit;
  const prefersLight = typeof matchMedia === 'function' && matchMedia('(prefers-color-scheme: light)').matches;
  return prefersLight ? 'light' : 'dark';
}

function paintThemeButton(btn) {
  const dark = currentTheme() === 'dark';
  const label = dark ? 'Switch to light theme' : 'Switch to dark theme';
  btn.setAttribute('aria-label', label);
  btn.setAttribute('title', label);
  btn.replaceChildren(icon(dark ? 'sun' : 'moon'));
}

function syncThemeButtons() {
  for (const btn of document.querySelectorAll('.theme-toggle')) paintThemeButton(btn);
}

function themeButton() {
  const btn = h('button', {
    type: 'button',
    class: 'icon-btn theme-toggle',
    onclick: () => {
      const next = currentTheme() === 'dark' ? 'light' : 'dark';
      applyTheme(next);
      storeTheme(next);
      syncThemeButtons();
    },
  });
  paintThemeButton(btn);
  return btn;
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
  }

  load(values) {
    this.confirmed = { ...values };
  }

  /** Records values saved elsewhere (the settings forms) so the overview switches match. */
  confirm(values) {
    Object.assign(this.confirmed, values);
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
          Object.assign(this.confirmed, res && res.values ? res.values : sent);
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

async function logout(btn) {
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
  location.reload();
}

function buildTopbar(menuBtn) {
  const user = state.user;
  const name = user ? user.globalName || user.username || 'Account' : '';
  return h(
    'header',
    { class: 'topbar' },
    menuBtn,
    h(
      'a',
      { class: 'brand', href: '#/' },
      h('img', { class: 'brand-mark', src: '/favicon.svg', alt: '', width: '28', height: '28' }),
      h('span', { class: 'brand-name' }, APP_NAME),
    ),
    h('span', { class: 'topbar-spacer' }),
    themeButton(),
    user
      ? h('div', { class: 'user' }, avatar(name, user.avatarUrl, { size: 'sm', round: true }), h('span', { class: 'user-name' }, name))
      : null,
    user
      ? h(
        'button',
        { type: 'button', class: 'btn btn-ghost btn-sm', onclick: (event) => logout(event.currentTarget) },
        icon('logout'),
        'Log out',
      )
      : null,
  );
}

/**
 * Logged-in page frame. With `sidebar` it also builds the server/section sidebar,
 * which turns into a slide-in drawer under 900px.
 */
function buildShell({ sidebar = false } = {}) {
  let drawerOpen = false;
  const main = h('main', { class: 'main', id: 'main', tabindex: '-1' });
  const serversSlot = h('div', { class: 'side-block' });
  const sectionsSlot = h('div', { class: 'side-block' });
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
        'aria-label': 'Servers and sections',
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
        'aria-label': 'Servers and sections',
        onkeydown: (event) => {
          if (event.key === 'Escape' && drawerOpen) setDrawer(false);
        },
        onclick: (event) => {
          if (event.target.closest && event.target.closest('a')) setDrawer(false);
        },
      },
      h('div', { class: 'sidebar-head' }, h('span', {}, 'Menu'), closeBtn),
      serversSlot,
      sectionsSlot,
    );
    backdrop = h('div', { class: 'drawer-backdrop', onclick: () => setDrawer(false) });
  }

  const skip = h('button', { type: 'button', class: 'skip-link', onclick: () => main.focus() }, 'Skip to content');
  const footer = h(
    'footer',
    { class: 'footer' },
    sidebar ? 'Module switches save as you flip them. Other settings save with the Save changes button.' : 'Choose a server to configure the bot.',
  );
  const root = h(
    'div',
    { class: 'app' },
    skip,
    buildTopbar(menuBtn),
    h('div', { class: sidebar ? 'layout has-sidebar' : 'layout' }, aside, h('div', { class: 'content' }, main, footer)),
    backdrop,
  );

  const shell = { root, main, serversSlot, sectionsSlot, setDrawer };
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

  const previewRow = (on) =>
    h(
      'div',
      { class: 'preview-row' },
      skeletonBlock('preview-emoji'),
      h('div', { class: 'preview-lines' }, skeletonBlock('skeleton-line w-50'), skeletonBlock('skeleton-line w-90')),
      h('span', { class: on ? 'mini-switch is-on' : 'mini-switch' }),
    );

  mount(
    h(
      'div',
      { class: 'hero-screen' },
      h('div', { class: 'hero-theme' }, themeButton()),
      h(
        'main',
        { class: 'hero', id: 'main', tabindex: '-1' },
        banners,
        h(
          'div',
          { class: 'hero-card' },
          h('img', { class: 'hero-logo', src: '/favicon.svg', alt: '', width: '64', height: '64' }),
          h('h1', { class: 'hero-title' }, 'Manage your server'),
          h(
            'p',
            { class: 'hero-lead' },
            'Log in with Discord to configure every module, channel, role and message of the bot for any server you manage.',
          ),
          h('a', { class: 'btn btn-primary btn-lg hero-cta', href: '/auth/login' }, 'Log in with Discord'),
          h('p', { class: 'hero-fine' }, 'We only read your profile and the list of your servers.'),
        ),
        h('div', { class: 'hero-preview', 'aria-hidden': 'true' }, previewRow(true), previewRow(true), previewRow(false)),
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
      h('div', { class: 'hero-theme' }, themeButton()),
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
    'span',
    { class: 'server-identity' },
    avatar(guild.name, guild.iconUrl, { size: 'md' }),
    h(
      'span',
      { class: 'server-text' },
      h('span', { class: 'server-name' }, guild.name),
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
      'a',
      { class: 'card server-card', href: `#/guild/${encodeURIComponent(guild.id)}` },
      serverIdentity(guild, guild.ownerAccess ? 'Bot owner access' : 'Bot is in this server'),
      h('span', { class: 'server-action' }, 'Manage', icon('forward')),
    );
  }
  const invite = safeHref(guild.inviteUrl);
  return h(
    'div',
    { class: 'card server-card is-dim' },
    serverIdentity(guild, 'Bot not added yet'),
    invite
      ? h(
        'a',
        { class: 'btn btn-primary btn-sm', href: invite, target: '_blank', rel: 'noopener' },
        icon('plus'),
        'Add bot',
        h('span', { class: 'sr-only' }, ` to ${guild.name}`),
      )
      : null,
  );
}

/** The bot owner's part of the picker: the servers the bot is in that are not in their own list. */
function ownerPickerSection(ownerGuilds) {
  if (!ownerGuilds || !ownerGuilds.length) return null;
  return h(
    'section',
    { class: 'owner-servers', 'aria-label': 'All servers the bot is in' },
    h(
      'div',
      { class: 'page-head' },
      h('h2', { class: 'page-title' }, 'All servers the bot is in'),
      h('p', { class: 'page-sub' }, 'Bot owner access: you can manage these even where you are not an admin or a member.'),
    ),
    h('div', { class: 'server-grid' }, ownerGuilds.map(serverCard)),
  );
}

function renderPickerBody(body, guilds) {
  const head = h(
    'div',
    { class: 'page-head' },
    h('h1', { class: 'page-title' }, 'Your servers'),
    h('p', { class: 'page-sub' }, 'Choose a server to manage its bot settings.'),
  );
  const owned = ownerPickerSection(state.ownerGuilds);
  if (!guilds.length && !owned) {
    body.replaceChildren(
      head,
      statusCard({
        tone: 'accent',
        iconName: 'server',
        title: 'No servers to manage',
        message:
          'You need the Manage Server permission on a server to configure the bot there. Ask a server admin to grant it, then reload this page.',
        actions: [retryButton(() => showPicker())],
      }),
    );
    return;
  }
  const mine = guilds.length
    ? h('div', { class: 'server-grid' }, guilds.map(serverCard))
    : h('p', { class: 'page-sub' }, 'You do not manage any servers yourself.');
  body.replaceChildren(head, mine, ...(owned ? [owned] : []));
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

  const body = h('div', { class: 'picker' });
  shell.main.append(body);
  if (state.guilds) renderPickerBody(body, state.guilds);
  else body.append(h('div', { class: 'page-head' }, skeletonBlock('boot-heading')), pickerSkeleton());
  shell.main.focus({ preventScroll: true });

  try {
    const guilds = await loadGuilds({ force: true });
    if (token !== routeToken) return;
    renderPickerBody(body, guilds);
  } catch (err) {
    if (token !== routeToken || handleAuthError(err)) return;
    if (state.guilds) toast(describeError(err).message, { kind: 'error', ms: TOAST_ERROR_MS });
    else renderPickerError(body, err);
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
  shell.serversSlot.append(h('h2', { class: 'side-title' }, 'Servers'), sidebarSkeleton());
  shell.sectionsSlot.append(h('h2', { class: 'side-title' }, 'Sections'), sidebarSkeleton());

  const guildsLoaded = loadGuilds().then(
    (guilds) => ({ guilds }),
    () => ({ guilds: null }),
  );
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

  const view = buildGuildView({ guildId, data: settingsRes.value, catalog: catalogRes.value, shell });
  currentView = view;
  shell.main.replaceChildren(view.main);
  document.title = `${view.guildName} · ${APP_NAME}`;
  shell.main.focus({ preventScroll: true });
  if (section) requestAnimationFrame(() => view.goTo(section));

  guildsLoaded.then(({ guilds }) => {
    if (token === routeToken) view.renderServers(guilds);
  });
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

/* ---- sidebar pieces shared by the overview and the settings pages ---- */

const guildHref = (guildId) => `#/guild/${encodeURIComponent(guildId)}`;
const modulePageHref = (guildId, key) => `${guildHref(guildId)}/module/${encodeURIComponent(key)}`;

function serverListItem(g, guildId) {
  if (g.botPresent) {
    const active = g.id === guildId;
    return h(
      'li',
      {},
      h(
        'a',
        {
          class: active ? 'side-item is-active' : 'side-item',
          href: guildHref(g.id),
          'aria-current': active ? 'page' : null,
        },
        avatar(g.name, g.iconUrl, { size: 'xs' }),
        h('span', { class: 'side-name' }, g.name),
      ),
    );
  }
  const invite = safeHref(g.inviteUrl);
  const inner = [
    avatar(g.name, g.iconUrl, { size: 'xs' }),
    h('span', { class: 'side-name' }, g.name),
    invite ? h('span', { class: 'side-plus' }, icon('plus')) : null,
    h('span', { class: 'sr-only' }, invite ? ' (add bot)' : ' (bot not added)'),
  ];
  return h(
    'li',
    {},
    invite
      ? h('a', { class: 'side-item is-dim', href: invite, target: '_blank', rel: 'noopener', title: 'Add the bot to this server' }, inner)
      : h('span', { class: 'side-item is-dim' }, inner),
  );
}

function renderServerList(slot, guilds, guildId) {
  slot.replaceChildren(
    h('h2', { class: 'side-title' }, 'Servers'),
    guilds
      ? h('nav', { 'aria-label': 'Servers' }, h('ul', { class: 'side-list server-list' }, guilds.map((g) => serverListItem(g, guildId))))
      : h('p', { class: 'side-note' }, 'Could not load your servers.'),
    ...(guilds && state.ownerGuilds?.length
      ? [
        h('h2', { class: 'side-title side-title-gap' }, 'Bot owner'),
        h('nav', { 'aria-label': 'All servers the bot is in' },
          h('ul', { class: 'side-list server-list' }, state.ownerGuilds.map((g) => serverListItem(g, guildId)))),
      ]
      : []),
    h('ul', { class: 'side-list' }, h('li', {}, h('a', { class: 'side-item', href: '#/' }, icon('back'), h('span', { class: 'side-name' }, 'All servers')))),
  );
}

/** "This server": the overview and the general settings page. `active` is 'overview' or 'general'. */
function serverNav(guildId, active) {
  const item = (id, href, iconName, label) =>
    h(
      'li',
      {},
      h(
        'a',
        {
          class: active === id ? (id === 'overview' ? 'side-item is-current' : 'side-item is-active') : 'side-item',
          href,
          'aria-current': active === id ? 'page' : null,
        },
        icon(iconName),
        h('span', { class: 'side-name' }, label),
      ),
    );
  return [
    h('h2', { class: 'side-title' }, 'This server'),
    h(
      'nav',
      { 'aria-label': 'Server pages' },
      h('ul', { class: 'side-list' }, item('overview', guildHref(guildId), 'home', 'Overview'), item('general', modulePageHref(guildId, 'general'), 'settings', 'General settings')),
    ),
  ];
}

/**
 * The module list for the settings pages: every module grouped by category, the current one highlighted, a dot
 * showing whether it is on. Returns { el, paint } (paint re-reads the on/off state from the store).
 */
function buildModuleNav({ guildId, model, store, currentKey }) {
  const links = [];
  const groups = model.categories.map((cat) =>
    h(
      'div',
      { class: 'side-group' },
      h('h3', { class: 'side-group-title' }, h('a', { href: `${guildHref(guildId)}/${encodeURIComponent(cat.id)}` }, cat.label)),
      h(
        'ul',
        { class: 'side-list' },
        cat.modules.map((mod) => {
          const dot = h('span', { class: 'status-dot' });
          const state = h('span', { class: 'sr-only' });
          const active = mod.key === currentKey;
          const link = h(
            'a',
            {
              class: active ? 'side-item side-module is-active' : 'side-item side-module',
              href: modulePageHref(guildId, mod.key),
              'aria-current': active ? 'page' : null,
            },
            h('span', { class: 'side-emoji', 'aria-hidden': 'true' }, mod.emoji || '\u{1F9E9}'),
            h('span', { class: 'side-name' }, mod.label),
            dot,
            state,
          );
          links.push({ mod, dot, state });
          return h('li', {}, link);
        }),
      ),
    ),
  );
  const paint = () => {
    for (const l of links) {
      const on = store.get(l.mod.flag, l.mod.default);
      l.dot.classList.toggle('is-on', on);
      l.state.textContent = on ? ' (on)' : ' (off)';
    }
  };
  return { el: h('nav', { 'aria-label': 'Modules' }, groups), paint };
}

/**
 * Builds the whole settings page for one guild and returns handles for the router.
 * Switches are painted from the GuildStore, so toggles never re-render the page.
 */
function buildGuildView({ guildId, data, catalog, shell }) {
  const store = getStore(guildId);
  store.load(data && typeof data.values === 'object' && data.values ? data.values : {});
  const model = normalizeCatalog(catalog);
  const guild = (data && data.guild) || { id: guildId, name: 'Server', iconUrl: null };
  const general = (data && data.general) || {};
  const propagation = Number(data && data.propagationSeconds) > 0 ? Number(data.propagationSeconds) : 30;
  const totalModules = model.categories.reduce((n, cat) => n + cat.modules.length, 0);

  const switches = []; // { el, field, fallback }
  const entries = new Map(); // module key -> entry
  const sections = [];
  const expanded = new Set();
  let query = '';
  let mode = 'all';

  const moduleOn = (mod) => store.get(mod.flag, mod.default);

  function makeSwitch(field, fallback, labelId, describedBy) {
    const el = createSwitch({ labelId, describedBy, onToggle: () => store.toggle(field, fallback) });
    switches.push({ el, field, fallback });
    return el;
  }

  /* ---- module cards ---- */

  function isOpen(entry) {
    return expanded.has(entry.mod.key) || entry.forced;
  }

  function paintOpen(entry) {
    if (!entry.collapse) return;
    const open = isOpen(entry);
    entry.collapse.classList.toggle('is-open', open);
    if (open) entry.collapse.removeAttribute('inert');
    else entry.collapse.setAttribute('inert', '');
    entry.disclosure.setAttribute('aria-expanded', String(open));
  }

  function paintExpandButton() {
    const expandable = [...entries.values()].filter((e) => e.collapse);
    expandAllBtn.hidden = expandable.length === 0;
    expandAllBtn.textContent = expandable.length > 0 && expandable.every(isOpen) ? 'Collapse all' : 'Expand all';
  }

  function toggleOpen(entry) {
    if (isOpen(entry)) {
      expanded.delete(entry.mod.key);
      entry.forced = false;
    } else {
      expanded.add(entry.mod.key);
    }
    paintOpen(entry);
    paintExpandButton();
  }

  function toggleAll() {
    const expandable = [...entries.values()].filter((e) => e.collapse);
    const collapse = expandable.every(isOpen);
    for (const entry of expandable) {
      if (collapse) {
        expanded.delete(entry.mod.key);
        entry.forced = false;
      } else {
        expanded.add(entry.mod.key);
      }
      paintOpen(entry);
    }
    paintExpandButton();
  }

  function buildEntry(mod) {
    const id = slug(mod.key);
    const labelId = `m-${id}-label`;
    const descId = `m-${id}-desc`;
    const subsId = `m-${id}-subs`;
    const entry = {
      mod,
      subs: [],
      forced: false,
      collapse: null,
      disclosure: null,
      note: null,
      ownText: [mod.label, mod.key, mod.description, mod.configCommand, fieldLabels(mod)].filter(Boolean).join(' ').toLowerCase(),
    };

    const toggle = makeSwitch(mod.flag, mod.default, labelId, mod.description ? descId : null);
    entry.card = h(
      'article',
      { class: 'card module is-off', 'data-module': mod.key },
      h(
        'div',
        { class: 'module-top' },
        h('span', { class: 'module-emoji', 'aria-hidden': 'true' }, mod.emoji || '\u{1F9E9}'),
        h(
          'div',
          { class: 'module-text' },
          h('h3', { class: 'module-title', id: labelId }, mod.label),
          mod.description ? h('p', { class: 'module-desc', id: descId }, mod.description) : null,
        ),
        toggle,
      ),
      h(
        'div',
        { class: 'module-links' },
        h(
          'a',
          { class: 'btn btn-secondary btn-sm module-settings', href: modulePageHref(guildId, mod.key) },
          icon('settings'),
          'Settings',
          h('span', { class: 'sr-only' }, ` for ${mod.label}`),
        ),
        mod.configCommand
          ? h('p', { class: 'module-hint', title: `Also available in Discord with ${mod.configCommand}` }, 'Discord: ', h('code', {}, mod.configCommand))
          : null,
      ),
    );

    if (mod.subToggles.length) {
      const list = h('ul', { class: 'sub-list' });
      for (const sub of mod.subToggles) {
        const sid = slug(sub.field);
        const subLabelId = `s-${sid}-label`;
        const subDescId = `s-${sid}-desc`;
        const subToggle = makeSwitch(sub.field, sub.default === true, subLabelId, sub.description ? subDescId : null);
        const row = h(
          'li',
          { class: 'sub' },
          h(
            'div',
            { class: 'sub-text' },
            h('span', { class: 'sub-label', id: subLabelId }, sub.label || sub.field),
            sub.description ? h('span', { class: 'sub-desc', id: subDescId }, sub.description) : null,
          ),
          subToggle,
        );
        list.append(row);
        entry.subs.push({ row, text: [sub.label, sub.description].filter(Boolean).join(' ').toLowerCase() });
      }
      entry.note = h('p', { class: 'sub-note' }, icon('info'), 'Applies when the module is on');
      entry.collapse = h('div', { class: 'collapse', id: subsId, inert: true }, h('div', { class: 'collapse-inner' }, entry.note, list));
      entry.disclosure = h(
        'button',
        {
          type: 'button',
          class: 'disclosure',
          'aria-expanded': 'false',
          'aria-controls': subsId,
          onclick: () => toggleOpen(entry),
        },
        h('span', {}, pluralize(mod.subToggles.length, 'option')),
        icon('chevron'),
      );
      entry.card.append(entry.disclosure, entry.collapse);
    }
    return entry;
  }

  for (const cat of model.categories) {
    const sectionId = `cat-${slug(cat.id)}`;
    const titleId = `${sectionId}-title`;
    const grid = h('div', { class: 'module-grid' });
    const section = {
      cat,
      id: cat.id,
      entries: [],
      countEl: h('span', { class: 'category-count num' }),
      badge: h('span', { class: 'badge' }),
    };
    for (const mod of cat.modules) {
      const entry = buildEntry(mod);
      section.entries.push(entry);
      entries.set(mod.key, entry);
      grid.append(entry.card);
    }
    section.el = h(
      'section',
      { class: 'category', id: sectionId, 'aria-labelledby': titleId },
      h('div', { class: 'category-head' }, h('h2', { class: 'category-title', id: titleId }, cat.label), section.countEl),
      cat.description ? h('p', { class: 'category-desc' }, cat.description) : null,
      grid,
    );
    const href = `#/guild/${encodeURIComponent(guildId)}/${encodeURIComponent(cat.id)}`;
    section.navLink = h(
      'a',
      {
        class: 'side-item',
        href,
        onclick: (event) => {
          // Same hash twice fires no hashchange, so scroll here.
          if (location.hash === href) {
            event.preventDefault();
            goTo(cat.id);
          }
        },
      },
      h('span', { class: 'side-name' }, cat.label),
      section.badge,
    );
    sections.push(section);
  }

  /* ---- header ---- */

  const summaryNum = h('strong', { class: 'num' }, '0');
  const progress = h('div', { class: 'progress', 'aria-hidden': 'true' }, h('div', { class: 'progress-bar' }));
  const saveStatus = h('span', { class: 'save-status', 'data-active': 'false', 'aria-hidden': 'true' }, h('span', { class: 'spinner' }), 'Saving…');

  function generalStrip() {
    const timezone = typeof general.timezone === 'string' && general.timezone ? general.timezone : 'UTC';
    const prefix =
      general.customPrefix === ''
        ? 'Off'
        : typeof general.customPrefix === 'string'
          ? h('code', {}, general.customPrefix)
          : '—';
    const colour = typeof general.embedColor === 'string' && /^#[0-9a-f]{6}$/i.test(general.embedColor) ? general.embedColor : null;
    const swatch = h('span', { class: 'swatch', 'aria-hidden': 'true' });
    if (colour) swatch.style.setProperty('--swatch', colour);
    const chip = (key, ...value) => h('span', { class: 'chip' }, h('span', { class: 'chip-k' }, key), h('span', { class: 'chip-v' }, ...value));
    return h(
      'div',
      { class: 'general', role: 'group', 'aria-label': 'General settings' },
      h('span', { class: 'general-title' }, 'General'),
      chip('Timezone', timezone),
      chip('Command prefix', prefix),
      chip('Embed colour', colour ? [swatch, colour.toUpperCase()] : 'Default'),
      h('a', { class: 'btn btn-secondary btn-sm general-edit', href: modulePageHref(guildId, 'general') }, icon('settings'), 'Edit general settings'),
    );
  }

  const header = h(
    'section',
    { class: 'card guild-header', 'aria-label': 'Server overview' },
    avatar(guild.name, guild.iconUrl, { size: 'lg' }),
    h(
      'div',
      { class: 'gh-text' },
      h('h1', { class: 'gh-name', title: guild.name }, guild.name),
      h('p', { class: 'gh-summary' }, summaryNum, ` of ${totalModules} modules on`),
      progress,
    ),
    saveStatus,
    h('p', { class: 'gh-note' }, icon('info'), `Changes reach the bot within ~${propagation} seconds.`),
    generalStrip(),
  );

  /* ---- toolbar ---- */

  const search = h('input', {
    type: 'search',
    class: 'search-input',
    placeholder: 'Search modules and options',
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

  const segCounts = {};
  const segButtons = {};
  const segmented = h(
    'div',
    { class: 'segmented', role: 'group', 'aria-label': 'Filter by status' },
    [['all', 'All'], ['on', 'On'], ['off', 'Off']].map(([id, label]) => {
      segCounts[id] = h('span', { class: 'num' }, '0');
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
        segCounts[id],
      );
      return segButtons[id];
    }),
  );

  const expandAllBtn = h('button', { type: 'button', class: 'btn btn-ghost btn-sm', onclick: toggleAll }, 'Expand all');

  const toolbar = h(
    'div',
    { class: 'toolbar-wrap' },
    h(
      'div',
      { class: 'toolbar', role: 'search' },
      h('div', { class: 'search' }, icon('search'), search, h('kbd', { class: 'kbd', 'aria-hidden': 'true' }, '/')),
      segmented,
      expandAllBtn,
    ),
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
    h('p', {}, 'No modules match your search or filter.'),
    h('button', { type: 'button', class: 'btn btn-secondary', onclick: clearFilters }, 'Clear filters'),
  );

  /* ---- filtering ---- */

  function applyFilters() {
    const tokens = query.toLowerCase().split(/\s+/).filter(Boolean);
    let shown = 0;
    for (const section of sections) {
      let sectionShown = 0;
      for (const entry of section.entries) {
        const everything = `${entry.ownText} ${entry.subs.map((s) => s.text).join(' ')}`;
        let visible = tokens.every((t) => everything.includes(t));
        if (visible && mode !== 'all') visible = moduleOn(entry.mod) === (mode === 'on');
        entry.card.hidden = !visible;

        // A hit that only exists in an option label opens the card and marks the row.
        const viaSub = visible && tokens.length > 0 && !tokens.every((t) => entry.ownText.includes(t));
        entry.forced = viaSub;
        for (const sub of entry.subs) sub.row.classList.toggle('is-match', viaSub && tokens.some((t) => sub.text.includes(t)));
        paintOpen(entry);
        if (visible) sectionShown++;
      }
      section.el.hidden = sectionShown === 0;
      section.navLink.classList.toggle('is-empty', sectionShown === 0);
      shown += sectionShown;
    }
    emptyState.hidden = shown !== 0;
    const filtering = tokens.length > 0 || mode !== 'all';
    resultNote.textContent = filtering ? `Showing ${shown} of ${totalModules} modules` : '';
    paintExpandButton();
  }

  /* ---- painting from the store ---- */

  function paint() {
    for (const s of switches) paintSwitch(s.el, store.get(s.field, s.fallback), store.isPending(s.field));

    let onTotal = 0;
    for (const section of sections) {
      let on = 0;
      for (const entry of section.entries) {
        const isOn = moduleOn(entry.mod);
        if (isOn) on++;
        entry.card.classList.toggle('is-on', isOn);
        entry.card.classList.toggle('is-off', !isOn);
        if (entry.note) entry.note.hidden = isOn;
      }
      onTotal += on;
      const count = section.entries.length;
      section.countEl.textContent = `${on} of ${count} on`;
      section.badge.textContent = `${on}/${count}`;
      section.badge.classList.toggle('has-on', on > 0);
    }
    summaryNum.textContent = String(onTotal);
    progress.style.setProperty('--pct', totalModules ? String(Math.round((onTotal / totalModules) * 100)) : '0');
    segCounts.all.textContent = String(totalModules);
    segCounts.on.textContent = String(onTotal);
    segCounts.off.textContent = String(totalModules - onTotal);
    saveStatus.setAttribute('data-active', String(store.busy));
  }

  /* ---- sidebar ---- */

  function setActive(active) {
    for (const section of sections) {
      const is = section === active;
      section.navLink.classList.toggle('is-active', is);
      if (is) section.navLink.setAttribute('aria-current', 'location');
      else section.navLink.removeAttribute('aria-current');
    }
  }

  function goTo(sectionId) {
    const section = sections.find((s) => s.id === sectionId);
    if (!sectionId || !section) {
      window.scrollTo({ top: 0 });
      return;
    }
    if (section.el.hidden) return;
    section.el.scrollIntoView({ block: 'start' });
    setActive(section);
  }

  shell.sectionsSlot.replaceChildren(
    ...serverNav(guildId, 'overview'),
    h('h2', { class: 'side-title side-title-gap' }, 'Sections'),
    h('nav', { 'aria-label': 'Sections' }, h('ul', { class: 'side-list' }, sections.map((s) => h('li', {}, s.navLink)))),
  );

  const renderServers = (guilds) => renderServerList(shell.serversSlot, guilds, guildId);

  // Scroll-spy for the section list.
  let observer = null;
  if (typeof IntersectionObserver === 'function') {
    const visibleSections = new Set();
    observer = new IntersectionObserver(
      (records) => {
        for (const record of records) {
          if (record.isIntersecting) visibleSections.add(record.target);
          else visibleSections.delete(record.target);
        }
        const first = sections.find((s) => visibleSections.has(s.el));
        if (first) setActive(first);
      },
      { rootMargin: '-150px 0px -55% 0px' },
    );
    for (const section of sections) observer.observe(section.el);
  }

  const main = h('div', { class: 'guild-view' }, header, toolbar, resultNote, emptyState, sections.map((s) => s.el));
  const unsubscribe = store.subscribe(paint);
  paint();
  applyFilters();
  activeSearch = search;
  if (state.guilds) renderServers(state.guilds);

  return {
    kind: 'overview',
    main,
    guildName: guild.name,
    goTo,
    renderServers,
    destroy() {
      unsubscribe();
      if (observer) observer.disconnect();
      if (activeSearch === search) activeSearch = null;
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
      label: general.title ? `${general.title} settings` : 'General settings',
      emoji: '⚙️',
      description: general.description || 'Time zone, embed colour and command prefix for this server.',
      groups: usable(general.groups),
      collections: [],
    };
  }
  const mod = model.categories.flatMap((cat) => cat.modules).find((m) => m.key === key);
  if (!mod) return null;
  const collections = (Array.isArray(mod.collections) ? mod.collections : []).filter((k) => catalog.collections && catalog.collections[k]);
  return { key, mod, label: mod.label, emoji: mod.emoji || '\u{1F9E9}', description: mod.description || '', groups: usable(mod.groups, [mod.flag]), collections };
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
  shell.serversSlot.append(h('h2', { class: 'side-title' }, 'Servers'), sidebarSkeleton());
  shell.sectionsSlot.append(h('h2', { class: 'side-title' }, 'Modules'), sidebarSkeleton());

  const base = `/api/guilds/${encodeURIComponent(guildId)}`;
  const guildsLoaded = loadGuilds().then(
    (guilds) => ({ guilds }),
    () => ({ guilds: null }),
  );
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
    shell.sectionsSlot.replaceChildren(...serverNav(guildId, null));
    guildsLoaded.then(({ guilds }) => {
      if (token === routeToken) renderServerList(shell.serversSlot, guilds, guildId);
    });
    return;
  }

  const resources = resourcesRes.status === 'fulfilled' && isResources(resourcesRes.value) ? resourcesRes.value : null;
  const view = buildSettingsView({ guildId, data: settingsRes.value, catalog, model, page, resources, shell });
  currentView = view;
  shell.main.replaceChildren(view.main);
  document.title = `${page.label} · ${view.guildName} · ${APP_NAME}`;
  shell.main.focus({ preventScroll: true });
  view.revealNav();

  guildsLoaded.then(({ guilds }) => {
    if (token === routeToken) view.renderServers(guilds);
  });
}

/**
 * Builds one module's (or the general) settings page: header with the on/off switch, one form per group, one
 * section per collection, and the unsaved-changes bar. Boolean flags are painted from the GuildStore, so the
 * switch here and the overview stay in sync; everything else is saved explicitly with ONE PATCH.
 */
function buildSettingsView({ guildId, data, catalog, model, page, resources, shell }) {
  const store = getStore(guildId);
  const values = data && typeof data.values === 'object' && data.values ? data.values : {};
  store.load(values);
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
    defaultEmbedColor: Number.isInteger(data?.values?.embedColor) ? data.values.embedColor : null,
    reloadResources: () => reloadResources(),
  };

  /* ---- resources (channels, roles, emojis) ---- */

  const entries = []; // one per group: { group, form, card }
  let resourcesPromise = null;

  function reloadResources() {
    if (resourcesPromise) return resourcesPromise;
    const promise = api(`/api/guilds/${encodeURIComponent(guildId)}/resources`)
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
        if (!destroyed) handleAuthError(err);
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
      .catch((err) => toast(describeError(err).message, { kind: 'error', key: 'resources-error', ms: TOAST_ERROR_MS }))
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
  const statePrefix = h('span', { class: 'mh-state-prefix' }, 'Module is ');
  const stateValue = h('span', {});
  const stateText = h('span', { class: 'mh-state' }, statePrefix, stateValue);
  const toggle = mod ? createSwitch({ labelId: titleId, describedBy: mod.description ? descId : null, onToggle: () => store.toggle(mod.flag, mod.default) }) : null;
  const offNotice = mod
    ? h('p', { class: 'mh-notice', hidden: true }, icon('info'), 'This module is off. Your settings are kept, but the bot ignores them until you switch it on.')
    : null;

  const header = h(
    'section',
    { class: 'card module-header', 'aria-labelledby': titleId },
    h('span', { class: 'mh-emoji', 'aria-hidden': 'true' }, page.emoji),
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
      h('p', { class: 'mh-note' }, icon('info'), `Changes reach the bot within ~${propagation} seconds.`),
      mod && mod.configCommand ? h('p', { class: 'mh-note' }, 'Also in Discord: ', h('code', {}, mod.configCommand)) : null,
      saveStatus,
    ),
    offNotice,
  );

  const crumbs = h(
    'nav',
    { class: 'crumbs', 'aria-label': 'Breadcrumb' },
    h('a', { class: 'crumb-link', href: guildHref(guildId) }, icon('back'), 'Overview'),
    h('span', { class: 'crumb-sep', 'aria-hidden': 'true' }, '/'),
    h('span', { class: 'crumb-here', 'aria-current': 'page' }, page.label),
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
  const countText = h('span', { class: 'save-bar-count num' });
  const resetBtn = h('button', { type: 'button', class: 'btn btn-ghost', onclick: () => reset() }, icon('undo'), 'Reset');
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
    h(
      'div',
      { class: 'save-bar-inner' },
      h('p', { class: 'save-bar-msg' }, h('span', { class: 'save-bar-dot', 'aria-hidden': 'true' }), h('span', {}, 'You have unsaved changes'), countText),
      h('div', { class: 'save-bar-actions' }, resetBtn, saveBtn),
    ),
  );

  const main = h(
    'div',
    { class: 'module-page' },
    crumbs,
    header,
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
    countText.textContent = any ? `· ${pluralize(dirty.size, 'setting')}` : '';
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

  async function save() {
    if (saving || dirty.size === 0 || destroyed) return;
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

  const moduleNav = buildModuleNav({ guildId, model, store, currentKey: mod ? mod.key : null });

  function paint() {
    if (mod) {
      const on = store.get(mod.flag, mod.default);
      paintSwitch(toggle, on, store.isPending(mod.flag));
      stateValue.textContent = on ? 'on' : 'off';
      header.classList.toggle('is-on', on);
      offNotice.hidden = on;
    }
    saveStatus.setAttribute('data-active', String(store.busy));
    moduleNav.paint();
  }

  shell.sectionsSlot.replaceChildren(
    ...serverNav(guildId, mod ? null : GENERAL_KEY),
    h('h2', { class: 'side-title side-title-gap' }, 'Modules'),
    moduleNav.el,
  );

  const unsubscribe = store.subscribe(paint);
  paint();
  paintBar();
  if (state.guilds) renderServerList(shell.serversSlot, state.guilds, guildId);

  return {
    kind: 'settings',
    main,
    guildName: guild.name,
    isDirty: () => dirty.size > 0,
    save,
    renderServers: (guilds) => renderServerList(shell.serversSlot, guilds, guildId),
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
  if (hasUnsavedChanges() && !window.confirm(LEAVE_PROMPT)) {
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

function start() {
  const stored = readStoredTheme();
  if (stored) applyTheme(stored);

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
  document.addEventListener('keydown', (event) => {
    if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey || String(event.key).toLowerCase() !== 's') return;
    if (!currentView || typeof currentView.save !== 'function') return;
    event.preventDefault();
    currentView.save();
  });

  window.addEventListener('beforeunload', (event) => {
    if (hasUnsavedChanges()) {
      event.preventDefault();
      event.returnValue = '';
      return;
    }
    for (const store of stores.values()) {
      if (store.busy) {
        event.preventDefault();
        event.returnValue = '';
        return;
      }
    }
  });

  if (typeof matchMedia === 'function') {
    matchMedia('(prefers-color-scheme: light)').addEventListener('change', syncThemeButtons);
    matchMedia('(min-width: 901px)').addEventListener('change', (event) => {
      if (event.matches && activeShell) activeShell.setDrawer(false);
    });
  }

  boot();
}

export { parseRoute, normalizeCatalog, initialsOf };

if (typeof document !== 'undefined') start();
