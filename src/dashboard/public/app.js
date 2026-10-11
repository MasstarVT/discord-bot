// Bot Dashboard: a dependency-free single-page UI (vanilla ES module, no build step).
//
// Layout of this file:
//   constants, DOM helpers, API client, state, theme, toasts, shared components,
//   the per-guild save queue, shell, screens, router, start().
//
// Nothing here touches the DOM, window or storage at import time (the module is
// import-checked in plain Node); everything runs from start() at the bottom.

const APP_NAME = 'Bot Dashboard';
const THEME_KEY = 'bot-dashboard-theme';
const SVG_NS = 'http://www.w3.org/2000/svg';
const SAVE_DEBOUNCE_MS = 120; // lets a double-click on one switch collapse into a no-op
const SAVED_TOAST_DELAY_MS = 350; // a burst of saves shows one "Saved" toast
const TOAST_MS = 3000;
const TOAST_ERROR_MS = 6000;
const MAX_TOASTS = 4;

// Stroke icons (24x24 grid); several sub-paths are joined with "|".
const ICONS = {
  menu: 'M4 6h16|M4 12h16|M4 18h16',
  search: 'M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14z|M21 21l-4.3-4.3',
  sun: 'M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8z|M12 2v2|M12 20v2|M4.9 4.9l1.4 1.4|M17.7 17.7l1.4 1.4|M2 12h2|M20 12h2|M4.9 19.1l1.4-1.4|M17.7 6.3l1.4-1.4',
  moon: 'M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z',
  chevron: 'M6 9l6 6 6-6',
  check: 'M20 6L9 17l-5-5',
  back: 'M19 12H5|M12 19l-7-7 7-7',
  forward: 'M5 12h14|M12 5l7 7-7 7',
  x: 'M18 6L6 18|M6 6l12 12',
  alert: 'M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z|M12 9v4|M12 17h.01',
  info: 'M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20z|M12 16v-4|M12 8h.01',
  refresh: 'M23 4v6h-6|M1 20v-6h6|M3.5 9a9 9 0 0 1 14.9-3.4L23 10|M1 14l4.6 4.4A9 9 0 0 0 20.5 15',
  logout: 'M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4|M16 17l5-5-5-5|M21 12H9',
  plus: 'M12 5v14|M5 12h14',
  lock: 'M5 11h14a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-8a1 1 0 0 1 1-1z|M8 11V7a4 4 0 0 1 8 0v4',
  server: 'M4 4h16a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z|M4 13h16a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-5a1 1 0 0 1 1-1z|M7 7.5h.01|M7 16.5h.01',
};

/* ------------------------------------------------------------------ */
/* DOM helpers                                                          */
/* ------------------------------------------------------------------ */

/**
 * h(tag, attrs, ...children): build an element.
 * Attributes: `class`, `onclick`-style listeners (lower-case event name), everything
 * else via setAttribute (true -> empty attribute, false/null/undefined skipped).
 * Children: strings and numbers become text nodes, so data is never parsed as markup.
 */
function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [name, value] of Object.entries(attrs)) {
      if (value === undefined || value === null || value === false) continue;
      if (name === 'class') el.className = value;
      else if (name.startsWith('on') && typeof value === 'function') el.addEventListener(name.slice(2), value);
      else el.setAttribute(name, value === true ? '' : String(value));
    }
  }
  appendAll(el, children);
  return el;
}

function appendAll(el, children) {
  for (const child of children) {
    if (child === undefined || child === null || child === false || child === true) continue;
    if (Array.isArray(child)) appendAll(el, child);
    else if (typeof Node !== 'undefined' && child instanceof Node) el.append(child);
    else el.append(document.createTextNode(String(child)));
  }
}

function icon(name) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  for (const d of (ICONS[name] || '').split('|')) {
    if (!d) continue;
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

function pluralize(n, one, many = `${one}s`) {
  return `${n} ${n === 1 ? one : many}`;
}

/** Make an arbitrary id usable inside an element id / URL fragment. */
function slug(value) {
  return String(value).replace(/[^A-Za-z0-9_-]/g, '-');
}

function safeUrl(url, protocols) {
  if (typeof url !== 'string' || !url) return null;
  try {
    const parsed = new URL(url, 'https://invalid.example');
    return protocols.includes(parsed.protocol) && /^https?:/i.test(url) ? parsed.href : null;
  } catch {
    return null;
  }
}
const safeHref = (url) => safeUrl(url, ['https:', 'http:']);
const safeImageUrl = (url) => safeUrl(url, ['https:']);

function mount(...nodes) {
  const app = document.getElementById('app');
  app.replaceChildren(...nodes);
  app.removeAttribute('aria-busy');
}

/* ------------------------------------------------------------------ */
/* API client                                                           */
/* ------------------------------------------------------------------ */

class ApiError extends Error {
  constructor(status, code, serverMessage, body, retryAfter = 0) {
    super(serverMessage || `Request failed (${status})`);
    this.name = 'ApiError';
    this.status = status; // 0 = the request never reached the server
    this.code = code;
    this.serverMessage = serverMessage || '';
    this.body = body;
    this.retryAfter = retryAfter;
  }
}

function fallbackMessage(status) {
  switch (status) {
    case 401: return 'Your session has expired. Please log in again.';
    case 403: return 'You no longer have Manage Server on this server.';
    case 404: return 'The bot is no longer in this server.';
    case 429: return 'Slow down, you are changing settings too quickly. Try again in a moment.';
    case 0: return 'Could not reach the dashboard. Check your connection and try again.';
    default: return 'Could not save, try again.';
  }
}

async function api(path, { method = 'GET', body } = {}) {
  const headers = { Accept: 'application/json' };
  const init = { method, credentials: 'same-origin', headers };
  if (method === 'GET') {
    init.cache = 'no-store';
  } else {
    headers['Content-Type'] = 'application/json';
    headers['X-Dashboard'] = '1';
    init.body = JSON.stringify(body ?? {});
  }

  let res;
  try {
    res = await fetch(path, init);
  } catch {
    throw new ApiError(0, 'network', fallbackMessage(0), null);
  }

  let data = null;
  if (res.status !== 204) {
    try { data = await res.json(); } catch { data = null; }
  }
  if (!res.ok) {
    const retryAfter = Number(data?.retryAfter ?? res.headers.get('Retry-After')) || 0;
    throw new ApiError(res.status, data?.error ?? 'error', data?.message ?? '', data, retryAfter);
  }
  return data;
}

/* ------------------------------------------------------------------ */
/* State                                                                */
/* ------------------------------------------------------------------ */

const state = {
  user: null,
  guilds: null, // /api/guilds, cached for the session
  guildsPromise: null,
  catalog: null, // /api/catalog, cached for the session
  catalogPromise: null,
  loginError: false,
};

let routeToken = 0; // bumped on every navigation so late responses can be dropped
let currentRoute = null;
let currentView = null;
let activeShell = null;
let activeSearch = null;
let toastRoot = null;

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

/* ------------------------------------------------------------------ */
/* Toasts                                                               */
/* ------------------------------------------------------------------ */

const toastKeys = new Map(); // key -> entry, so repeated messages refresh one toast
const toastEntries = [];

function getToastRoot() {
  if (!toastRoot || !toastRoot.isConnected) {
    toastRoot = h('div', { class: 'toasts', role: 'status', 'aria-live': 'polite' });
    document.body.append(toastRoot);
  }
  return toastRoot;
}

function dismissToast(entry) {
  clearTimeout(entry.timer);
  if (entry.key && toastKeys.get(entry.key) === entry) toastKeys.delete(entry.key);
  const at = toastEntries.indexOf(entry);
  if (at !== -1) toastEntries.splice(at, 1);
  entry.el.classList.add('is-out');
  setTimeout(() => entry.el.remove(), 200);
}

function toast(message, { kind = 'info', key = null, ms = TOAST_MS } = {}) {
  let entry = key ? toastKeys.get(key) : null;
  if (entry) {
    clearTimeout(entry.timer);
    entry.msg.textContent = message;
  } else {
    const msg = h('span', { class: 'toast-msg' }, message);
    const el = h(
      'div',
      { class: `toast toast-${kind}` },
      icon(kind === 'error' ? 'alert' : kind === 'success' ? 'check' : 'info'),
      msg,
      h('button', { type: 'button', class: 'toast-close', 'aria-label': 'Dismiss', onclick: () => dismissToast(entry) }, icon('x')),
    );
    entry = { el, msg, key, timer: 0 };
    toastEntries.push(entry);
    if (key) toastKeys.set(key, entry);
    getToastRoot().append(el);
    while (toastEntries.length > MAX_TOASTS) dismissToast(toastEntries[0]);
  }
  entry.timer = setTimeout(() => dismissToast(entry), ms);
}

/* ------------------------------------------------------------------ */
/* Shared components                                                    */
/* ------------------------------------------------------------------ */

function initialsOf(name) {
  const words = String(name || '?').trim().split(/\s+/).filter(Boolean);
  const first = Array.from(words[0] || '?')[0];
  const last = words.length > 1 ? Array.from(words[words.length - 1])[0] : '';
  return (first + last).toUpperCase();
}

function hueOf(name) {
  let hash = 0;
  for (const ch of String(name)) hash = (hash * 31 + ch.codePointAt(0)) >>> 0;
  return hash % 8;
}

/** Icon image, or coloured initials when there is no (loadable) image. */
function avatar(name, url, { size = 'md', round = false } = {}) {
  const wrap = h('span', { class: `avatar avatar-${size} avatar-c${hueOf(name)}${round ? ' avatar-round' : ''}` });
  const src = safeImageUrl(url);
  const showInitials = () => wrap.replaceChildren(document.createTextNode(initialsOf(name)));
  if (src) {
    const img = h('img', { src, alt: '', loading: 'lazy', decoding: 'async', referrerpolicy: 'no-referrer' });
    img.addEventListener('error', showInitials, { once: true });
    wrap.append(img);
  } else {
    showInitials();
  }
  return wrap;
}

function banner(kind, text) {
  return h(
    'div',
    { class: `banner banner-${kind}`, role: kind === 'error' ? 'alert' : 'status' },
    icon(kind === 'error' ? 'alert' : 'info'),
    h('span', {}, text),
  );
}

function statusCard({ tone = 'neutral', iconName = 'alert', title, message, actions = [], role = null }) {
  return h(
    'div',
    { class: 'card status-card', role },
    h('div', { class: `status-icon tone-${tone}` }, icon(iconName)),
    h('h1', { class: 'status-title' }, title),
    message ? h('p', { class: 'status-msg' }, message) : null,
    actions.length ? h('div', { class: 'status-actions' }, actions) : null,
  );
}

function backLink() {
  return h('a', { class: 'btn btn-secondary', href: '#/' }, icon('back'), 'Back to servers');
}

function retryButton(onRetry) {
  return h('button', { type: 'button', class: 'btn btn-primary', onclick: onRetry }, icon('refresh'), 'Try again');
}

function describeError(err) {
  switch (err.status) {
    case 429:
      return {
        title: 'Slow down a little',
        message: err.retryAfter
          ? `Too many requests. Please try again in ${pluralize(err.retryAfter, 'second')}.`
          : 'Too many requests. Please wait a moment and try again.',
      };
    case 502:
      return { title: 'Discord is not responding', message: 'We could not reach Discord just now. This is usually temporary.' };
    case 503:
      return { title: 'Settings are temporarily unavailable', message: 'The bot database is not answering right now. Please try again in a moment.' };
    case 0:
      return { title: 'Cannot reach the dashboard', message: fallbackMessage(0) };
    default:
      return { title: 'Something went wrong', message: err.serverMessage || 'An unexpected error occurred. Please try again.' };
  }
}

/** True (and the login screen is shown) when the error is a 401. */
function handleAuthError(err, notice = '') {
  if (!err || err.status !== 401) return false;
  renderLoggedOut({ notice });
  return true;
}

/* ---- Switch ---- */

function createSwitch({ labelId, describedBy = null, onToggle }) {
  return h(
    'button',
    {
      type: 'button',
      class: 'switch',
      role: 'switch',
      'aria-checked': 'false',
      'aria-labelledby': labelId,
      'aria-describedby': describedBy,
      onclick: onToggle,
    },
    h('span', { class: 'switch-track' }, h('span', { class: 'switch-thumb' })),
  );
}

function paintSwitch(el, on, pending) {
  el.setAttribute('aria-checked', on ? 'true' : 'false');
  el.classList.toggle('is-pending', pending);
  if (pending) el.setAttribute('aria-busy', 'true');
  else el.removeAttribute('aria-busy');
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
    if (open) closeBtn.focus();
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
  const footer = h('footer', { class: 'footer' }, 'Changes are saved automatically.');
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
  if (typeof document !== 'undefined') document.body.classList.remove('no-scroll');
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
          h('p', { class: 'hero-lead' }, 'Log in with Discord to switch bot modules and their options on or off for any server you manage.'),
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
    h('span', { class: 'server-text' }, h('span', { class: 'server-name' }, guild.name), h('span', { class: 'server-meta' }, meta)),
  );
}

function serverCard(guild) {
  if (guild.botPresent) {
    return h(
      'a',
      { class: 'card server-card', href: `#/guild/${encodeURIComponent(guild.id)}` },
      serverIdentity(guild, 'Bot is in this server'),
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

function renderPickerBody(body, guilds) {
  const head = h(
    'div',
    { class: 'page-head' },
    h('h1', { class: 'page-title' }, 'Your servers'),
    h('p', { class: 'page-sub' }, 'Choose a server to manage its bot settings.'),
  );
  if (!guilds.length) {
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
  body.replaceChildren(head, h('div', { class: 'server-grid' }, guilds.map(serverCard)));
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

function showGuildError(guildId, section, err) {
  const shell = buildShell({ sidebar: false });
  mount(shell.root);
  document.title = APP_NAME;
  const retry = retryButton(() => showGuild(guildId, section));
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
    showGuildError(guildId, section, failure);
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
      ownText: [mod.label, mod.key, mod.description, mod.configCommand].filter(Boolean).join(' ').toLowerCase(),
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
      mod.configCommand ? h('p', { class: 'module-hint' }, 'Configure in Discord with ', h('code', {}, mod.configCommand)) : null,
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
      { class: 'general', role: 'group', 'aria-label': 'General settings (read-only)' },
      h('span', { class: 'general-title' }, 'General'),
      chip('Timezone', timezone),
      chip('Command prefix', prefix),
      chip('Embed colour', colour ? [swatch, colour.toUpperCase()] : 'Default'),
      h('span', { class: 'general-hint' }, 'Read-only here. Change these in Discord with ', h('code', {}, '/settings'), '.'),
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
    h('h2', { class: 'side-title' }, 'Sections'),
    h('nav', { 'aria-label': 'Sections' }, h('ul', { class: 'side-list' }, sections.map((s) => h('li', {}, s.navLink)))),
  );

  function serverItem(g) {
    if (g.botPresent) {
      const active = g.id === guildId;
      return h(
        'li',
        {},
        h(
          'a',
          {
            class: active ? 'side-item is-active' : 'side-item',
            href: `#/guild/${encodeURIComponent(g.id)}`,
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

  function renderServers(guilds) {
    shell.serversSlot.replaceChildren(
      h('h2', { class: 'side-title' }, 'Servers'),
      guilds
        ? h('nav', { 'aria-label': 'Servers' }, h('ul', { class: 'side-list server-list' }, guilds.map(serverItem)))
        : h('p', { class: 'side-note' }, 'Could not load your servers.'),
      h('ul', { class: 'side-list' }, h('li', {}, h('a', { class: 'side-item', href: '#/' }, icon('back'), h('span', { class: 'side-name' }, 'All servers')))),
    );
  }

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
/* Router                                                               */
/* ------------------------------------------------------------------ */

/** `#/guild/<id>` or `#/guild/<id>/<category>`; anything else is the picker. */
function parseRoute(hash) {
  const match = /^#\/guild\/(\d{1,30})(?:\/([A-Za-z0-9_-]{1,40}))?\/?$/.exec(hash || '');
  if (match) return { name: 'guild', id: match[1], section: match[2] || null };
  return { name: 'picker', id: null, section: null };
}

function route() {
  if (!state.user) return;
  const next = parseRoute(location.hash);
  if (next.name === 'guild' && currentRoute && currentRoute.name === 'guild' && currentRoute.id === next.id && currentView) {
    currentView.goTo(next.section); // same server: only the section anchor changed
    return;
  }
  currentRoute = next;
  if (next.name === 'guild') showGuild(next.id, next.section);
  else showPicker();
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

  window.addEventListener('hashchange', route);

  // "/" focuses the search box, unless the user is already typing somewhere.
  document.addEventListener('keydown', (event) => {
    if (event.key !== '/' || event.ctrlKey || event.metaKey || event.altKey || !activeSearch) return;
    const target = event.target;
    if (target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) return;
    event.preventDefault();
    activeSearch.focus();
    activeSearch.select();
  });

  window.addEventListener('beforeunload', (event) => {
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
