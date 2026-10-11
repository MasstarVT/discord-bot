// Shared front-end helpers for the dashboard: DOM building, icons, the JSON API client, toasts and small
// components (avatar, banner, status card, switch). Imported by app.js and the page modules.
//
// Nothing here touches the DOM, window or storage at import time (the module is import-checked in plain
// Node); every function does its work only when called.

const SVG_NS = 'http://www.w3.org/2000/svg';
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
/* Toasts                                                               */
/* ------------------------------------------------------------------ */

let toastRoot = null;
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

export {
  ICONS, TOAST_MS, TOAST_ERROR_MS,
  h, appendAll, icon, pluralize, slug, safeUrl, safeHref, safeImageUrl, mount,
  ApiError, fallbackMessage, api,
  toast, dismissToast,
  initialsOf, hueOf, avatar, banner, statusCard, backLink, retryButton, describeError,
  createSwitch, paintSwitch,
};
