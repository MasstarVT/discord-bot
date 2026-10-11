// Per-server record lists of the web dashboard (saved embeds, custom commands, level rewards, ...):
//   mountCollection(container, { collection, ctx }) -> { destroy(), refresh() }
// A list with a count and an empty state, an editor dialog built with createForm (forms.js), actions with their own
// small forms, and delete with a confirmation. Searchable lists get a search box and "Load more" pages; member lists
// show an avatar per row; actions sit on the row (or in a "⋯" menu) and may be answered by the bot, so a slow
// request says it is waiting. API: /api/guilds/:id/records/:key (dashboard contract, section 5 and wave 2).
//
// Import-safe in plain Node: nothing touches document, window or storage until a function is called.

import { h, icon, api, ApiError, toast, describeError, canonical, slug, safeHref, avatar, TOAST_ERROR_MS } from './lib.js';
import { createForm, createPopover } from './forms.js';
import { templateSummary, sampleText, uiIcon, colorHex } from './embed-editor.js';

const SEARCH_DEBOUNCE_MS = 250;
const SEARCH_MAX_LENGTH = 100;
const WAIT_HINT_MS = 1000; // a write slower than this says it is waiting for the bot
const WAIT_LABEL = 'Waiting for the bot…';
const ROW_BUTTONS_MAX = 2; // more plain actions than this go into a "⋯" menu

let dialogSeq = 0;
let openModals = 0;

/* ------------------------------------------------------------------ */
/* Dialogs                                                              */
/* ------------------------------------------------------------------ */

// Toasts live in a fixed box at the end of <body>, but a modal <dialog> is drawn in the top layer above everything
// else, so while dialogs are open the box is moved into the topmost one (and back when it closes).
function placeToasts() {
  const box = document.querySelector('.toasts');
  if (!box) return;
  const target = [...document.querySelectorAll('dialog.cl-dialog[open]')].at(-1) ?? document.body;
  if (box.parentElement !== target) target.append(box);
}

function notify(message, opts) {
  toast(message, opts);
  placeToasts();
}

/**
 * A modal <dialog>: the page behind it is inert, Escape and the close button ask `onRequestClose` (which decides,
 * e.g. after a "discard changes?" question), focus returns to the opener when it closes.
 */
function openModal({ title, subtitle = null, description = null, wide = false, size = '', onRequestClose }) {
  dialogSeq += 1;
  const id = `cl-dlg-${dialogSeq}`;
  const titleEl = h('h2', { class: 'cl-dlg-title', id: `${id}-title` }, title);
  const subEl = h('p', { class: 'cl-dlg-sub', hidden: !subtitle }, subtitle ?? '');
  const descEl = description ? h('p', { class: 'cl-dlg-desc', id: `${id}-desc` }, description) : null;
  const closeBtn = h('button', { type: 'button', class: 'icon-btn cl-dlg-close', 'aria-label': 'Close' }, icon('x'));
  const body = h('div', { class: 'cl-dlg-body' });
  const foot = h('div', { class: 'cl-dlg-foot' });
  const dlg = h(
    'dialog',
    {
      class: `cl-dialog${wide ? ' is-wide' : ''}${size ? ` is-${size}` : ''}`,
      'aria-labelledby': `${id}-title`,
      'aria-describedby': descEl ? `${id}-desc` : null,
    },
    h('div', { class: 'cl-dlg-head' }, h('div', { class: 'cl-dlg-heading' }, titleEl, subEl, descEl), closeBtn),
    body,
    foot,
  );
  const opener = document.activeElement;
  let closed = false;
  const request = () => {
    if (!closed) onRequestClose();
  };
  closeBtn.addEventListener('click', request);
  // Escape is handled on keydown so the browser never closes the dialog behind our back (a control that used the
  // key itself, like an open list box, calls preventDefault and is left alone).
  dlg.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || e.defaultPrevented) return;
    e.preventDefault();
    request();
  });
  dlg.addEventListener('cancel', (e) => {
    e.preventDefault();
    request();
  });
  dlg.addEventListener('close', () => {
    if (closed) return;
    // Closed by the browser anyway: show it again and ask properly.
    try {
      dlg.showModal();
    } catch { /* detached */ }
    request();
  });
  document.body.append(dlg);
  openModals += 1;
  document.body.classList.add('no-scroll');
  dlg.showModal();
  placeToasts();
  return {
    el: dlg,
    body,
    foot,
    setTitle(text, sub = null) {
      titleEl.textContent = text;
      subEl.textContent = sub ?? '';
      subEl.hidden = !sub;
    },
    close({ restoreFocus = true } = {}) {
      if (closed) return;
      closed = true;
      try {
        dlg.close();
      } catch { /* already closed */ }
      placeToasts();
      dlg.remove();
      openModals = Math.max(0, openModals - 1);
      if (!openModals) document.body.classList.remove('no-scroll');
      if (restoreFocus && opener && opener.isConnected && typeof opener.focus === 'function') opener.focus();
    },
    get closed() {
      return closed;
    },
  };
}

/** A yes/no question in a small dialog. Resolves true when confirmed. */
function confirmModal({ title, message, confirmLabel = 'Confirm', cancelLabel = 'Cancel', danger = false }) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (answer) => {
      if (done) return;
      done = true;
      modal.close();
      resolve(answer);
    };
    const modal = openModal({ title, size: 'small', onRequestClose: () => finish(false) });
    modal.el.setAttribute('role', 'alertdialog');
    const msgId = `cl-confirm-${dialogSeq}`;
    modal.el.setAttribute('aria-describedby', msgId);
    modal.body.append(h('p', { class: 'cl-confirm-msg', id: msgId }, message));
    const cancel = h('button', { type: 'button', class: 'btn btn-secondary', onclick: () => finish(false) }, cancelLabel);
    const ok = h('button', { type: 'button', class: `btn ${danger ? 'cl-btn-danger' : 'btn-primary'}`, onclick: () => finish(true) },
      confirmLabel);
    modal.foot.append(h('span', { class: 'cl-foot-spacer' }), cancel, ok);
    (danger ? cancel : ok).focus();
  });
}

/* ------------------------------------------------------------------ */
/* Busy buttons and the row menu                                        */
/* ------------------------------------------------------------------ */

const waits = new WeakMap(); // button -> its pending "waiting for the bot" hint

/**
 * Marks a button busy (disabled, spinner). Once the request has taken longer than WAIT_HINT_MS the button says it is
 * waiting for the bot (dashboard actions run in the bot and can take several seconds); an icon-only button gets the
 * words next to it instead.
 */
function busy(btn, on, { hint = true, announce = null } = {}) {
  if (!btn) return;
  const before = waits.get(btn);
  if (before) {
    clearTimeout(before.timer);
    before.restore();
    waits.delete(btn);
  }
  btn.disabled = on;
  btn.classList.toggle('is-busy', on);
  if (on) btn.setAttribute('aria-busy', 'true');
  else btn.removeAttribute('aria-busy');
  if (!on || !hint) return;
  const entry = { timer: 0, restore() {} };
  entry.timer = setTimeout(() => {
    if (!btn.isConnected) return;
    announce?.(WAIT_LABEL);
    if (btn.classList.contains('icon-btn')) {
      const tag = h('span', { class: 'cl-wait' }, WAIT_LABEL);
      btn.before(tag);
      entry.restore = () => tag.remove();
      return;
    }
    const saved = [...btn.childNodes];
    const label = document.createTextNode(WAIT_LABEL);
    btn.replaceChildren(label);
    // The caller may have repainted the button meanwhile (a new label): then leave it alone.
    entry.restore = () => {
      if (btn.firstChild === label && btn.childNodes.length === 1) btn.replaceChildren(...saved);
    };
  }, WAIT_HINT_MS);
  waits.set(btn, entry);
}

function moreIcon() {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  for (const x of [5, 12, 19]) {
    const dot = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    dot.setAttribute('d', `M${x} 12h.01`);
    svg.append(dot);
  }
  return svg;
}

/**
 * A menu button ("⋯") with a popup menu of actions (WAI-ARIA menu button pattern): Enter, Space or the arrow keys open it
 * and focus an item, arrows/Home/End/first letters move, Enter or Space picks, Escape closes and returns to the button,
 * Tab or a click elsewhere just closes.
 * @param {{ id: string, label: string, entries: { label: string, description?: string, danger?: boolean,
 *   run: () => void }[] }} opts
 * @returns {{ el: HTMLElement, button: HTMLButtonElement, close: (opts?: { restoreFocus?: boolean }) => void }}
 */
function createRowMenu({ id, label, entries }) {
  const button = h('button', {
    type: 'button',
    class: 'icon-btn cl-menu-btn',
    'aria-haspopup': 'menu',
    'aria-expanded': 'false',
    'aria-controls': id,
    'aria-label': label,
    title: 'Actions',
  }, moreIcon());
  const list = h('div', { class: 'cl-menu', role: 'menu', id, 'aria-label': label });
  const wrap = h('span', { class: 'cl-menu-wrap' }, button);
  const pop = createPopover(button, {
    root: wrap, className: 'cl-menu-pop', onDismiss: () => close({ restoreFocus: false }), minWidth: 210, align: 'end',
  });
  pop.el.append(list);
  // Keep the focus where it is while the pointer works the menu (Safari does not focus clicked buttons).
  pop.el.addEventListener('mousedown', (event) => event.preventDefault());
  wrap.append(pop.el);

  const items = [];
  let separated = false;
  for (const entry of entries) {
    if (entry.danger && !separated && items.length) {
      list.append(h('div', { class: 'cl-menu-sep', role: 'separator' }));
      separated = true;
    }
    const item = h('button', {
      type: 'button',
      class: `cl-menu-item${entry.danger ? ' is-danger' : ''}`,
      role: 'menuitem',
      tabindex: '-1',
      title: entry.description || null,
    }, entry.label);
    item.addEventListener('click', () => {
      // Focus goes back to the button first, so a dialog the action opens returns there when it closes.
      close();
      entry.run();
    });
    items.push(item);
    list.append(item);
  }

  function close({ restoreFocus = true } = {}) {
    if (!pop.isOpen) return;
    pop.close();
    button.setAttribute('aria-expanded', 'false');
    if (restoreFocus) button.focus();
  }
  function open(at = 0) {
    if (pop.isOpen || button.disabled) return;
    pop.open();
    button.setAttribute('aria-expanded', 'true');
    items[at === -1 ? items.length - 1 : at]?.focus();
  }
  button.addEventListener('click', () => (pop.isOpen ? close() : open()));
  button.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      open(event.key === 'ArrowUp' ? -1 : 0);
    }
  });
  list.addEventListener('keydown', (event) => {
    const at = items.indexOf(document.activeElement);
    let next = null;
    switch (event.key) {
      case 'ArrowDown': next = (at + 1) % items.length; break;
      case 'ArrowUp': next = (at - 1 + items.length) % items.length; break;
      case 'Home': next = 0; break;
      case 'End': next = items.length - 1; break;
      case 'Escape':
        event.preventDefault();
        event.stopPropagation();
        close();
        return;
      case 'Tab':
        close({ restoreFocus: false });
        return;
      default:
        if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
          const letter = event.key.toLowerCase();
          const order = [...items.slice(at + 1), ...items.slice(0, at + 1)];
          next = items.indexOf(order.find((i) => i.textContent.trim().toLowerCase().startsWith(letter)));
          if (next === -1) next = null;
        }
        break;
    }
    if (next === null) return;
    event.preventDefault();
    items[next].focus();
  });
  wrap.addEventListener('focusout', (event) => {
    if (!event.relatedTarget || !wrap.contains(event.relatedTarget)) close({ restoreFocus: false });
  });

  return { el: wrap, button, close };
}

/* ------------------------------------------------------------------ */
/* Value formatting                                                     */
/* ------------------------------------------------------------------ */

const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const clone = (v) => (v === undefined ? undefined : structuredClone(v));

function lookups(ctx) {
  const res = isObj(ctx?.resources) ? ctx.resources : null;
  return {
    loaded: Boolean(res),
    channels: new Map((Array.isArray(res?.channels) ? res.channels : []).map((ch) => [ch.id, ch])),
    roles: new Map((Array.isArray(res?.roles) ? res.roles : []).map((r) => [r.id, r])),
  };
}

function channelName(id, look) {
  const ch = look.channels.get(id);
  if (ch) return `${ch.kind === 'voice' || ch.kind === 'stage' ? '🔊 ' : '#'}${ch.name}`;
  return look.loaded ? 'deleted channel' : `channel …${String(id).slice(-4)}`;
}

function roleName(id, look) {
  const r = look.roles.get(id);
  if (r) return r.everyone ? '@everyone' : `@${r.name}`;
  return look.loaded ? 'deleted role' : `role …${String(id).slice(-4)}`;
}

function listText(parts, max = 3) {
  if (!parts.length) return '—';
  return parts.length > max ? `${parts.slice(0, max).join(', ')} +${parts.length - max}` : parts.join(', ');
}

function formatDate(iso, withTime = true) {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return '—';
  try {
    return new Intl.DateTimeFormat(undefined, withTime
      ? { dateStyle: 'medium', timeStyle: 'short' }
      : { dateStyle: 'medium' }).format(new Date(t));
  } catch {
    return new Date(t).toISOString();
  }
}

/** A short text for a value of any descriptor type (list rows and the info list). */
function formatValue(desc, value, ctx, look = lookups(ctx)) {
  if (value === null || value === undefined || value === '') return '—';
  switch (desc?.type) {
    case 'boolean': return value ? 'On' : 'Off';
    case 'int':
    case 'float': return `${Number(value).toLocaleString()}${desc.unit ? ` ${desc.unit}` : ''}`;
    case 'enum': return desc.options?.find((o) => o.value === value)?.label ?? String(value);
    case 'multiEnum': {
      const label = (v) => desc.options?.find((o) => o.value === v)?.label ?? String(v);
      return listText((Array.isArray(value) ? value : []).map(label));
    }
    case 'color': return typeof value === 'number' ? colorHex(value) : String(value);
    case 'channel': return channelName(value, look);
    case 'channels': return listText((Array.isArray(value) ? value : []).map((id) => channelName(id, look)));
    case 'role': return roleName(value, look);
    case 'roles': return listText((Array.isArray(value) ? value : []).map((id) => roleName(id, look)));
    case 'template':
    case 'embed': {
      const s = templateSummary(desc.type === 'embed' ? { content: null, embed: value, buttons: [] } : value, ctx);
      return s.title || (s.hasEmbed ? 'Embed' : '—');
    }
    case 'stringList': return listText(Array.isArray(value) ? value.map(String) : []);
    case 'objectList': return Array.isArray(value) ? `${value.length} ${value.length === 1 ? 'entry' : 'entries'}` : '—';
    case 'object': return '…';
    case 'datetime': return formatDate(value);
    case 'url': {
      try {
        const u = new URL(String(value));
        return `${u.hostname}${u.pathname === '/' ? '' : u.pathname}`;
      } catch {
        return String(value);
      }
    }
    case 'text':
      return desc.multiline ? sampleText(String(value), ctx).split('\n').find((l) => l.trim()) ?? '—' : String(value);
    default: return typeof value === 'object' ? '…' : String(value);
  }
}

/* ------------------------------------------------------------------ */
/* Errors of writes                                                     */
/* ------------------------------------------------------------------ */

// Where a failed write is shown: 'reload' (session gone), 'field' (a form field), 'status' (next to the dialog's
// buttons; without a dialog, in an error toast). Discord's refusals (502, e.g. a missing permission) and the bot's
// (422, `action_failed`) are 'status'.
function writeError(err, c, limit) {
  if (!(err instanceof ApiError)) return { where: 'status', message: 'Something went wrong. Please try again.' };
  const msg = err.serverMessage;
  switch (err.status) {
    case 401: return { where: 'reload', message: 'Your session has expired. Reloading…' };
    case 400: {
      const path = typeof err.body?.path === 'string' ? err.body.path : '';
      return { where: path ? 'field' : 'status', path, message: msg || 'Please check the highlighted fields.' };
    }
    case 409: return {
      where: 'status',
      message: msg || (err.code === 'limit' && Number.isInteger(limit)
        ? `You have reached the limit of ${limit} ${c.label.toLowerCase()}.`
        : 'That conflicts with an existing entry.'),
    };
    case 502: return { where: 'status', message: msg || "Discord refused the request. Check the bot's permissions and try again." };
    // The bot ran the action and refused it ("That member is above the bot."): its reason is the message.
    case 422: return { where: 'status', message: msg || 'The bot could not do that.' };
    case 404: return {
      where: 'status',
      gone: err.code === 'not_found',
      message: err.code === 'bot_not_in_guild'
        ? 'The bot is no longer in this server.'
        : msg || `This ${c.singular} no longer exists.`,
    };
    case 413: return { where: 'status', message: 'This is too large to save. Shorten the text and try again.' };
    case 403: return { where: 'status', message: msg || 'You no longer have Manage Server on this server.' };
    default: return { where: 'status', message: msg || describeError(err).message };
  }
}

/* ------------------------------------------------------------------ */
/* The collection                                                       */
/* ------------------------------------------------------------------ */

function normalizeCollection(raw) {
  const c = isObj(raw) ? raw : {};
  const arr = (v) => (Array.isArray(v) ? v.filter(isObj) : []);
  return {
    key: String(c.key ?? 'records'),
    label: String(c.label ?? 'Items'),
    singular: String(c.singular ?? 'item'),
    description: String(c.description ?? ''),
    limit: Number.isInteger(c.limit) ? c.limit : null, // null = no cap per server: the list shows just a count
    titleField: String(c.titleField ?? 'id'),
    subtitleField: c.subtitleField ? String(c.subtitleField) : null,
    avatarField: c.avatarField ? String(c.avatarField) : null,
    searchable: c.searchable === true,
    searchPlaceholder: c.searchPlaceholder ? String(c.searchPlaceholder) : null,
    canCreate: c.canCreate !== false,
    canEdit: c.canEdit !== false,
    canDelete: c.canDelete !== false,
    fields: arr(c.fields),
    createOnly: Array.isArray(c.createOnly) ? c.createOnly.map(String) : [],
    readOnly: arr(c.readOnly),
    actions: arr(c.actions).map((a) => ({ ...a, fields: arr(a.fields), danger: a.danger === true, confirm: a.confirm || null })),
  };
}

const defaultsOf = (fields) => Object.fromEntries(fields.map((f) => [f.field, clone(f.default ?? null)]));

/**
 * Mounts the list of one collection into `container`.
 * @param {HTMLElement} container
 * @param {{ collection: object, ctx: object }} opts collection = catalog.collections[key]; ctx = the page context
 * @returns {{ destroy: () => void, refresh: () => Promise<void> }}
 */
export function mountCollection(container, { collection, ctx = {} } = {}) {
  const c = normalizeCollection(collection);
  const base = `/api/guilds/${encodeURIComponent(ctx.guildId ?? '')}/records/${encodeURIComponent(c.key)}`;
  const idBase = `cl-${slug(c.key)}`;
  const tplField = c.fields.find((f) => f.type === 'template' || f.type === 'embed') ?? null;
  const fieldByName = new Map([...c.fields, ...c.readOnly].map((f) => [f.field, f]));
  const publishable = c.readOnly.some((f) => f.field === 'channelId') && c.readOnly.some((f) => f.field === 'messageId');
  const isPosted = (item) => Boolean(item?.channelId && item?.messageId);
  // Actions on the posted copy (update it, delete it) mean nothing for a draft.
  const actionApplies = (a, item) => !(publishable && !isPosted(item) && ['refresh', 'unpublish', 'update'].includes(a.id));
  const placeholder = c.searchPlaceholder || `Search ${c.label.toLowerCase()}`;
  let items = [];
  let limit = c.limit;
  let query = ''; // the applied search text
  let nextCursor = null; // set while the server has more pages
  let phase = 'loading'; // loading | ready | error
  let loadErr = null;
  let destroyed = false;
  let loadSeq = 0;
  let loadingMore = false;
  let formSeq = 0;
  let searchTimer = 0;
  const menus = []; // the row menus of the rendered rows
  const modals = new Set();
  let editor = null; // the open editor (see openEditor)

  /* ---- shell ---- */

  const countEl = h('span', { class: 'cl-count num', 'aria-live': 'polite' });
  const limitNote = h('span', { class: 'cl-limit-note', hidden: true });
  const reloadBtn = h('button', {
    type: 'button', class: 'icon-btn cl-reload', 'aria-label': `Reload ${c.label.toLowerCase()}`, title: 'Reload',
  }, icon('refresh'));
  const createBtn = h('button', { type: 'button', class: 'btn btn-primary btn-sm cl-create' }, icon('plus'), `New ${c.singular}`);
  const listEl = h('ul', { class: 'cl-list', 'aria-label': c.label });
  const bodyEl = h('div', { class: 'cl-body' });
  const liveEl = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
  const announce = (text) => {
    liveEl.textContent = text;
  };

  const searchInput = c.searchable
    ? h('input', {
      type: 'search',
      class: 'cl-search-input',
      id: `${idBase}-search`,
      placeholder,
      'aria-label': placeholder,
      autocomplete: 'off',
      autocapitalize: 'off',
      spellcheck: 'false',
      enterkeyhint: 'search',
      maxlength: String(SEARCH_MAX_LENGTH),
    })
    : null;
  const searchClear = c.searchable
    ? h('button', { type: 'button', class: 'icon-btn cl-search-clear', 'aria-label': 'Clear search', hidden: true }, icon('x'))
    : null;
  const searchEl = c.searchable
    ? h('div', { class: 'cl-search', role: 'search', 'aria-label': `Search ${c.label.toLowerCase()}` },
      icon('search'), searchInput, searchClear)
    : null;
  const moreBtn = h('button', { type: 'button', class: 'btn btn-secondary cl-more-btn' }, 'Load more');
  const moreEl = h('div', { class: 'cl-more', hidden: true }, moreBtn);
  const root = h(
    'div',
    { class: 'cl', 'data-collection': c.key },
    h('div', { class: 'cl-toolbar' },
      countEl, limitNote, h('span', { class: 'cl-spacer' }), reloadBtn, c.canCreate ? createBtn : null),
    searchEl,
    bodyEl,
    moreEl,
    liveEl,
  );
  container.append(root);
  reloadBtn.addEventListener('click', () => load());
  createBtn.addEventListener('click', () => openEditor(null));
  moreBtn.addEventListener('click', () => loadMore());

  // A list with a cap counts "n / limit"; one without ("limit: null") just counts. A search counts its matches. The "+"
  // marks a list with more pages than are loaded.
  const atLimit = () => Number.isInteger(limit) && !query && !nextCursor && items.length >= limit;

  function countText() {
    const n = items.length;
    const more = nextCursor ? '+' : '';
    const one = n === 1 && !more;
    if (query) return `${n}${more} ${one ? 'match' : 'matches'}`;
    if (Number.isInteger(limit)) return `${n}${more} / ${limit}`;
    return `${n}${more} ${one ? c.singular : c.label.toLowerCase()}`;
  }

  function paintToolbar() {
    countEl.textContent = phase === 'ready' ? countText() : '';
    const full = phase === 'ready' && atLimit();
    createBtn.disabled = phase !== 'ready' || full;
    limitNote.hidden = !full;
    limitNote.textContent = full ? 'Limit reached. Delete one to add another.' : '';
    reloadBtn.disabled = phase === 'loading';
    if (searchEl) {
      searchClear.hidden = !searchInput.value;
      searchEl.classList.toggle('has-text', Boolean(searchInput.value));
    }
  }

  function paintMore() {
    moreEl.hidden = !(phase === 'ready' && nextCursor);
    moreBtn.disabled = loadingMore;
    moreBtn.classList.toggle('is-busy', loadingMore);
    moreBtn.textContent = loadingMore ? 'Loading…' : 'Load more';
    if (loadingMore) moreBtn.setAttribute('aria-busy', 'true');
    else moreBtn.removeAttribute('aria-busy');
  }

  /* ---- rows ---- */

  function itemTitle(item) {
    const desc = fieldByName.get(c.titleField);
    const v = item?.[c.titleField];
    if (v === undefined || v === null || v === '') {
      return c.titleField === 'id' ? String(item?.id ?? '') : `Untitled ${c.singular}`;
    }
    return desc ? formatValue(desc, v, ctx) : String(v);
  }

  function statusNode(item, look) {
    if (publishable) {
      const posted = isPosted(item);
      let text = 'Draft';
      if (posted) text = `Posted in ${channelName(item.channelId, look)}`;
      else if (item.channelId) text = `Draft · last in ${channelName(item.channelId, look)}`;
      return h('span', { class: `cl-status${posted ? ' is-live' : ''}` }, h('span', { class: 'cl-dot', 'aria-hidden': 'true' }), text);
    }
    if (!c.subtitleField) return null;
    const desc = fieldByName.get(c.subtitleField);
    const v = item[c.subtitleField];
    if (desc?.type === 'boolean') {
      return h('span', { class: `cl-badge${v ? ' is-on' : ''}` }, `${desc.label}: ${v ? 'on' : 'off'}`);
    }
    const text = desc ? formatValue(desc, v, ctx, look) : String(v ?? '');
    return text && text !== '—' ? h('span', { class: 'cl-sub-text' }, text) : null;
  }

  // Only a switched-off item is flagged; every other boolean stays in the editor (rows would get noisy).
  function badgesFor(item) {
    const enabled = c.fields.find((f) => f.field === 'enabled' && f.type === 'boolean');
    const off = enabled && item.enabled === false && c.subtitleField !== 'enabled';
    return off ? [h('span', { class: 'cl-badge is-off' }, 'Disabled')] : [];
  }

  function miniPreview(item) {
    if (!tplField) return null;
    const v = item[tplField.field];
    const s = templateSummary(tplField.type === 'embed' ? { content: null, embed: v, buttons: [] } : v, ctx);
    const mini = h('span', { class: `cl-mini${s.color ? '' : ' is-plain'}`, 'aria-hidden': 'true' },
      h('span', { class: 'cl-mini-title' }, s.title || 'Empty message'),
      h('span', { class: 'cl-mini-lines' }, h('span'), h('span')));
    if (s.color) mini.style.setProperty('--embed-color', s.color);
    return mini;
  }

  // A round picture (member lists); initials when the item has none or it does not load.
  function rowAvatar(item, title) {
    if (!c.avatarField) return null;
    const url = typeof item[c.avatarField] === 'string' ? item[c.avatarField] : null;
    return h('span', { class: 'cl-row-avatar' }, avatar(title, url, { size: 'md', round: true }));
  }

  // The actions on a row: every plain (non-danger) action as a button while there are at most two of them, else a
  // "⋯" menu with all of them, dangerous ones last. Dangerous actions are always in the details dialog too.
  function rowActions(item, title, titleId) {
    const applicable = c.actions.filter((a) => actionApplies(a, item));
    const plain = applicable.filter((a) => !a.danger);
    if (!plain.length) return null;
    if (plain.length <= ROW_BUTTONS_MAX) {
      return {
        menu: false,
        el: h('div', { class: 'cl-row-actions' }, plain.map((a) => {
          const btn = h('button', {
            type: 'button',
            class: 'btn btn-secondary btn-sm cl-row-action',
            'aria-describedby': titleId,
            title: a.description || null,
          }, a.id === 'publish' ? uiIcon('send') : null, a.label);
          btn.addEventListener('click', () => runAction(a, item, btn));
          return btn;
        })),
      };
    }
    const sorted = [...plain, ...applicable.filter((a) => a.danger)];
    const menu = createRowMenu({
      id: `${titleId}-menu`,
      label: `Actions for ${title}`,
      entries: sorted.map((a) => ({
        label: a.label, description: a.description, danger: a.danger, run: () => runAction(a, item, menu.button),
      })),
    });
    return { menu, el: h('div', { class: 'cl-row-actions' }, menu.el) };
  }

  function rowFor(item, look) {
    const title = itemTitle(item);
    const titleId = `${idBase}-r-${slug(item.id)}`;
    const sub = statusNode(item, look);
    const badges = badgesFor(item);
    const main = h(
      'button',
      { type: 'button', class: 'cl-row-main', 'aria-labelledby': titleId, title: c.canEdit ? `Edit ${title}` : `View ${title}` },
      rowAvatar(item, title) ?? miniPreview(item),
      h('span', { class: 'cl-row-text' },
        h('span', { class: 'cl-row-title', id: titleId }, title),
        sub || badges.length ? h('span', { class: 'cl-row-sub' }, badges, sub) : null),
      h('span', { class: 'cl-row-chevron', 'aria-hidden': 'true' }, icon('forward')),
    );
    main.addEventListener('click', () => openEditor(item));
    const actions = rowActions(item, title, titleId);
    if (actions?.menu) menus.push(actions.menu);
    return h('li', { class: `cl-row${actions?.menu ? ' has-menu' : ''}`, 'data-id': item.id }, main, actions?.el);
  }

  function closeMenus() {
    for (const menu of menus.splice(0)) menu.close({ restoreFocus: false });
  }

  function skeleton() {
    return h('ul', { class: 'cl-list', 'aria-hidden': 'true' }, [0, 1, 2].map(() => h('li', { class: 'cl-row is-skeleton' },
      h('span', { class: `skeleton ${c.avatarField ? 'cl-skel-avatar' : 'cl-skel-mini'}` }),
      h('span', { class: 'cl-skel-text' },
        h('span', { class: 'skeleton skeleton-line w-50' }),
        h('span', { class: 'skeleton skeleton-line w-30' })))));
  }

  function emptyState() {
    return h(
      'div',
      { class: 'cl-empty' },
      h('span', { class: 'cl-empty-icon', 'aria-hidden': 'true' }, tplField ? uiIcon('layers') : icon('plus')),
      h('p', { class: 'cl-empty-title' }, `No ${c.label.toLowerCase()} yet`),
      c.canCreate ? h('p', { class: 'cl-empty-text' }, `Create your first ${c.singular} to get started.`) : null,
      c.canCreate
        ? h('button', { type: 'button', class: 'btn btn-primary', onclick: () => openEditor(null) },
          icon('plus'), `Create ${c.singular}`)
        : null,
    );
  }

  function noMatchesState() {
    return h(
      'div',
      { class: 'cl-empty is-search' },
      h('span', { class: 'cl-empty-icon', 'aria-hidden': 'true' }, icon('search')),
      h('p', { class: 'cl-empty-title' }, 'No matches'),
      h('p', { class: 'cl-empty-text' }, `Nothing in ${c.label.toLowerCase()} matches "${clampQuery(query)}".`),
      h('button', { type: 'button', class: 'btn btn-secondary btn-sm', onclick: () => clearSearch() }, 'Clear search'),
    );
  }

  const clampQuery = (text) => (text.length > 40 ? `${text.slice(0, 40)}…` : text);

  function render({ focusId = null } = {}) {
    if (destroyed) return;
    closeMenus();
    paintToolbar();
    paintMore();
    root.setAttribute('aria-busy', phase === 'loading' ? 'true' : 'false');
    listEl.classList.remove('is-stale');
    listEl.removeAttribute('aria-busy');
    searchEl?.classList.remove('is-busy');
    if (phase === 'loading') {
      bodyEl.replaceChildren(skeleton());
      return;
    }
    if (phase === 'error') {
      const message = loadErr instanceof ApiError ? describeError(loadErr).message : 'The list could not be loaded.';
      bodyEl.replaceChildren(h('div', { class: 'cl-error', role: 'alert' }, icon('alert'),
        h('div', { class: 'cl-error-text' }, h('strong', {}, `Could not load ${c.label.toLowerCase()}`), h('span', {}, message)),
        h('button', { type: 'button', class: 'btn btn-secondary btn-sm', onclick: () => load() }, icon('refresh'), 'Try again')));
      return;
    }
    if (!items.length) {
      bodyEl.replaceChildren(query ? noMatchesState() : emptyState());
      return;
    }
    const look = lookups(ctx);
    listEl.replaceChildren(...items.map((item) => rowFor(item, look)));
    bodyEl.replaceChildren(listEl);
    if (focusId !== null) listEl.querySelector(`[data-id="${CSS.escape(focusId)}"] .cl-row-main`)?.focus();
  }

  const readItems = (data) => (Array.isArray(data?.items) ? data.items.filter((x) => isObj(x) && typeof x.id === 'string') : []);
  const readCursor = (data) => (typeof data?.nextCursor === 'string' && data.nextCursor ? data.nextCursor : null);

  function listUrl(cursor = null) {
    const params = [];
    if (query) params.push(`query=${encodeURIComponent(query)}`);
    if (cursor) params.push(`cursor=${encodeURIComponent(cursor)}`);
    return params.length ? `${base}?${params.join('&')}` : base;
  }

  /**
   * Loads the first page (of the current search). `fresh`: the request is about a different question than the list on
   * screen (a new search), so a failure replaces the list by an error instead of leaving stale rows.
   */
  async function load({ fresh = false } = {}) {
    const seq = ++loadSeq;
    loadingMore = false;
    if (!items.length || phase === 'error') {
      phase = 'loading';
      render();
    } else {
      // Keep the rows on screen (dimmed) while the answer is on its way.
      listEl.classList.add('is-stale');
      listEl.setAttribute('aria-busy', 'true');
      searchEl?.classList.add('is-busy');
      moreEl.hidden = true;
    }
    reloadBtn.disabled = true;
    try {
      const data = await api(listUrl());
      if (destroyed || seq !== loadSeq) return;
      items = readItems(data);
      nextCursor = readCursor(data);
      if (data && Object.hasOwn(data, 'limit')) limit = Number.isInteger(data.limit) ? data.limit : null;
      phase = 'ready';
      loadErr = null;
    } catch (err) {
      if (destroyed || seq !== loadSeq) return;
      if (err instanceof ApiError && err.status === 401) {
        location.reload();
        return;
      }
      if (phase !== 'ready' || fresh) {
        phase = 'error';
        loadErr = err;
        items = [];
        nextCursor = null;
      } else {
        notify(`Could not reload ${c.label.toLowerCase()}: ${describeError(err).message}`, { kind: 'error', ms: TOAST_ERROR_MS });
      }
    }
    render({ focusId: focusedRowId() });
  }

  // Appends the next page below the rows on screen; focus moves to its first row.
  async function loadMore() {
    if (loadingMore || !nextCursor || phase !== 'ready') return;
    const seq = loadSeq;
    loadingMore = true;
    paintMore();
    try {
      const data = await api(listUrl(nextCursor));
      if (destroyed || seq !== loadSeq) return;
      const known = new Set(items.map((x) => x.id));
      const added = readItems(data).filter((x) => !known.has(x.id));
      items.push(...added);
      nextCursor = readCursor(data);
      loadingMore = false;
      const look = lookups(ctx);
      const rows = added.map((item) => rowFor(item, look));
      listEl.append(...rows);
      paintToolbar();
      paintMore();
      announce(`${added.length} more loaded. ${countText()}`);
      rows[0]?.querySelector('.cl-row-main')?.focus();
    } catch (err) {
      if (destroyed || seq !== loadSeq) return;
      loadingMore = false;
      if (err instanceof ApiError && err.status === 401) {
        location.reload();
        return;
      }
      const reason = describeError(err).message;
      notify(`Could not load more ${c.label.toLowerCase()}: ${reason}`, { kind: 'error', ms: TOAST_ERROR_MS });
      paintMore();
    }
  }

  /* ---- search ---- */

  const normalizeQuery = (text) => text.replace(/\s+/g, ' ').trim().slice(0, SEARCH_MAX_LENGTH);

  // Applies the search text (same text = nothing to do); an empty text is the normal list.
  function setQuery(text) {
    clearTimeout(searchTimer);
    searchTimer = 0;
    const next = normalizeQuery(text);
    paintToolbar();
    if (next === query) return;
    query = next;
    load({ fresh: true });
  }

  function clearSearch() {
    if (searchInput) {
      searchInput.value = '';
      searchInput.focus();
    }
    setQuery('');
  }

  if (searchInput) {
    searchInput.addEventListener('input', () => {
      paintToolbar();
      clearTimeout(searchTimer);
      searchTimer = setTimeout(() => setQuery(searchInput.value), SEARCH_DEBOUNCE_MS);
    });
    searchInput.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        setQuery(searchInput.value);
      } else if (event.key === 'Escape' && searchInput.value) {
        event.preventDefault();
        event.stopPropagation();
        clearSearch();
      }
    });
    searchClear.addEventListener('click', () => clearSearch());
  }

  const focusedRowId = () => (listEl.contains(document.activeElement)
    ? document.activeElement.closest('[data-id]')?.dataset.id ?? null
    : null);

  function upsert(item, { prepend = false } = {}) {
    if (!isObj(item) || typeof item.id !== 'string') return;
    const at = items.findIndex((x) => x.id === item.id);
    if (at === -1) {
      if (prepend) items.unshift(item);
      else items.push(item);
    } else {
      items[at] = item;
    }
    render({ focusId: focusedRowId() });
  }

  function dropItem(id) {
    items = items.filter((x) => x.id !== id);
    render();
  }

  function focusRow(id) {
    const btn = listEl.querySelector(`[data-id="${CSS.escape(String(id))}"] .cl-row-main`);
    if (btn) btn.focus();
    else (createBtn.isConnected && !createBtn.disabled ? createBtn : reloadBtn).focus();
  }

  /* ---- forms ---- */

  function routeFieldError(forms, path, message) {
    const head = String(path).split('.')[0];
    const hit = forms.find((f) => f.names.has(head));
    if (!hit) return false;
    try {
      hit.form.setErrors([{ path, message }]);
      hit.form.focusPath(path);
    } catch { /* a control without error support still gets the status line */ }
    return true;
  }

  function setStatus(el, message, kind = 'error') {
    el.textContent = message || '';
    el.hidden = !message;
    el.classList.toggle('is-ok', kind === 'ok');
  }

  /* ---- actions ---- */

  // The bot answers `still working` (HTTP 200 with that message) when a task outlasted the dashboard's wait.
  const isPending = (res, message) => res?.pending === true || res?.status === 'pending' || /\bstill working\b/i.test(message);

  function toastResult(res, fallback) {
    const message = typeof res?.message === 'string' && res.message ? res.message : fallback;
    if (isPending(res, message)) notify(message, { kind: 'info', ms: TOAST_ERROR_MS });
    else notify(message, { kind: 'success' });
  }

  async function postAction(action, item, data) {
    const url = `${base}/${encodeURIComponent(item.id)}/actions/${encodeURIComponent(action.id)}`;
    const res = await api(url, { method: 'POST', body: { data } });
    toastResult(res, `${action.label}: done.`);
    if (isObj(res?.item)) {
      upsert(res.item);
      if (editor && editor.itemId() === res.item.id) editor.updateItem(res.item);
      return res.item;
    }
    if (res && res.item === null) {
      // The answer carries no item (the action may have removed it): ask the server what the list looks like now.
      resync(item.id);
      return null;
    }
    upsert(item);
    return item;
  }

  // Reloads the list; an editor still open on an item that is gone (in a list that is not searched or paged) closes.
  async function resync(itemId) {
    await load();
    const gone = phase === 'ready' && !query && !nextCursor && !items.some((x) => x.id === itemId);
    if (!destroyed && gone && editor && editor.itemId() === itemId) editor.close();
  }

  // After a dialog that started from a row: the row may have been redrawn, so make sure focus is not lost.
  function keepFocus(itemId) {
    if (itemId && (!document.activeElement || document.activeElement === document.body)) focusRow(itemId);
  }

  async function runAction(action, item, trigger, { fromEditor = null } = {}) {
    let target = item;
    if (fromEditor && fromEditor.isDirty()) {
      const saved = await fromEditor.save({ keepOpen: true });
      if (!saved) return;
      target = saved;
    }
    if (!action.fields.length) {
      if (action.confirm) {
        const ok = await confirmModal({
          title: action.label, message: action.confirm, confirmLabel: action.label, danger: action.danger,
        });
        if (!ok) return;
      }
      busy(trigger, true, { announce });
      try {
        await postAction(action, target, {});
      } catch (err) {
        const info = writeError(err, c, limit);
        if (info.where === 'reload') location.reload();
        else if (fromEditor) fromEditor.showError(info.message); // the dialog shows the refusal next to its buttons
        else notify(info.message, { kind: 'error', ms: TOAST_ERROR_MS });
        if (info.gone) load();
      } finally {
        busy(trigger, false);
        if (!fromEditor) keepFocus(item.id);
      }
      return;
    }
    openActionDialog(action, target);
  }

  function openActionDialog(action, item) {
    let form = null;
    const modal = openModal({
      title: action.label,
      subtitle: itemTitle(item),
      description: action.description || null,
      size: 'medium',
      onRequestClose: () => close(),
    });
    modals.add(modal);
    const close = () => {
      modals.delete(modal);
      try {
        form?.destroy();
      } catch { /* ignore */ }
      modal.close();
    };
    const values = {};
    for (const f of action.fields) {
      const own = item[f.field];
      values[f.field] = own !== undefined && own !== null && own !== '' ? clone(own) : clone(f.default ?? null);
    }
    formSeq += 1;
    try {
      form = createForm({ fields: action.fields, values, ctx, idPrefix: `${idBase}-a${formSeq}` });
    } catch {
      modal.body.append(h('p', { class: 'cl-dlg-status' }, 'This form could not be displayed. Reload the page and try again.'));
    }
    if (action.confirm) modal.body.append(h('p', { class: 'cl-note is-warn' }, icon('alert'), h('span', {}, action.confirm)));
    if (form) modal.body.append(form.el);
    const status = h('p', { class: 'cl-dlg-status', role: 'alert', hidden: true });
    const cancel = h('button', { type: 'button', class: 'btn btn-secondary', onclick: () => close() }, 'Cancel');
    const run = h('button', { type: 'button', class: `btn ${action.danger ? 'cl-btn-danger' : 'btn-primary'}`, disabled: !form },
      action.id === 'publish' ? uiIcon('send') : null, action.label);
    modal.foot.append(status, h('span', { class: 'cl-foot-spacer' }), cancel, run);
    run.addEventListener('click', async () => {
      if (!form) return;
      setStatus(status, '');
      try {
        form.setErrors([]);
      } catch { /* ignore */ }
      busy(run, true, { announce });
      try {
        await postAction(action, item, form.getValues());
        close();
        keepFocus(item.id);
      } catch (err) {
        const info = writeError(err, c, limit);
        if (info.where === 'reload') {
          location.reload();
          return;
        }
        const target = [{ form, names: new Set(action.fields.map((f) => f.field)) }];
        if (info.where === 'field' && routeFieldError(target, info.path, info.message)) {
          setStatus(status, 'Please fix the highlighted field.');
        } else {
          setStatus(status, info.message);
        }
        if (info.gone) load();
      } finally {
        busy(run, false);
      }
    });
    queueFocus(modal.body.querySelector('input, select, textarea, button, [tabindex]') ?? run);
  }

  /* ---- delete ---- */

  async function deleteItem(item, trigger, { fromEditor = null } = {}) {
    const title = itemTitle(item);
    const posted = publishable && isPosted(item);
    const ok = await confirmModal({
      title: `Delete this ${c.singular}?`,
      message: `"${title}" will be deleted. This cannot be undone.${posted
        ? ' The posted message stays in Discord; delete the posted message first if you want it gone too.' : ''}`,
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return false;
    busy(trigger, true, { announce });
    try {
      await api(`${base}/${encodeURIComponent(item.id)}`, { method: 'DELETE' });
      dropItem(item.id);
      notify(`Deleted "${title}".`, { kind: 'success' });
      return true;
    } catch (err) {
      const info = writeError(err, c, limit);
      if (info.where === 'reload') location.reload();
      else if (fromEditor) fromEditor.showError(info.message);
      else notify(info.message, { kind: 'error', ms: TOAST_ERROR_MS });
      if (info.gone) {
        dropItem(item.id);
        return true;
      }
      return false;
    } finally {
      busy(trigger, false);
    }
  }

  /* ---- editor ---- */

  function queueFocus(el) {
    if (!el) return;
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(() => el.focus());
    else el.focus();
  }

  function openEditor(item) {
    if (editor) return;
    if (!item && (!c.canCreate || atLimit())) return;
    let mode = item ? 'edit' : 'create';
    let current = item;
    let forms = []; // { form, names }
    let mainForm = null;
    let baseline = '';
    let baseValues = {};
    let saving = false;
    const wide = Boolean(tplField);
    const modal = openModal({
      title: item ? `${c.canEdit ? 'Edit' : 'View'} ${c.singular}` : `New ${c.singular}`,
      subtitle: item ? itemTitle(item) : null,
      wide,
      onRequestClose: () => requestClose(),
    });
    modals.add(modal);

    const status = h('p', { class: 'cl-dlg-status', role: 'alert', hidden: true });
    const dirtyNote = h('span', { class: 'cl-dirty', hidden: true },
      h('span', { class: 'cl-dot', 'aria-hidden': 'true' }), 'Unsaved changes');
    const saveBtn = h('button', { type: 'button', class: 'btn btn-primary cl-save' });
    const cancelBtn = h('button', { type: 'button', class: 'btn btn-secondary' }, 'Cancel');
    const deleteBtn = h('button', { type: 'button', class: 'btn btn-ghost cl-delete' }, uiIcon('trash'), 'Delete');
    cancelBtn.addEventListener('click', () => requestClose());
    saveBtn.addEventListener('click', () => save());
    deleteBtn.addEventListener('click', async () => {
      if (!current) return;
      const gone = await deleteItem(current, deleteBtn, { fromEditor: ctl });
      if (gone) finish();
    });

    const valuesNow = () => (mainForm ? mainForm.getValues() : {});
    const isDirty = () => Boolean(mainForm) && canonical(valuesNow()) !== baseline;
    function paintDirty() {
      const dirty = isDirty();
      dirtyNote.hidden = !dirty || mode === 'create';
      if (mode === 'edit') saveBtn.disabled = saving || !dirty;
    }

    // A collection whose items cannot be edited opens a details view: the facts and the actions, no form.
    const detailsOnly = () => mode === 'edit' && !c.canEdit;

    function infoSection() {
      if (mode !== 'edit' || !current) return null;
      const look = lookups(ctx);
      const rows = [];
      const shownFields = detailsOnly()
        ? [
          ...c.fields.filter((f) => current[f.field] !== undefined && !c.readOnly.some((r) => r.field === f.field)),
          ...c.readOnly,
        ]
        : c.readOnly;
      for (const d of shownFields) {
        const v = current[d.field];
        if (v === null || v === undefined || v === '' || d.field === c.avatarField) continue;
        if (c.avatarField && d.field === c.titleField) continue; // the header above shows the name
        let shown;
        if (d.type === 'url') {
          const href = safeHref(String(v));
          const text = /discord\.com\/channels\//.test(String(v)) ? 'Open in Discord' : formatValue(d, v, ctx, look);
          shown = href ? h('a', { href, target: '_blank', rel: 'noopener noreferrer' }, text, uiIcon('external')) : '—';
        } else if (d.field === 'status' && publishable) {
          shown = statusNode(current, look);
        } else if (d.type === 'text' && !/Id$/.test(d.field)) {
          shown = String(v).slice(0, 2000); // free text (a reason, a note) in full, wrapped
        } else if (d.type === 'stringList' && Array.isArray(v) && v.length) {
          shown = v.slice(0, 50).map(String).join('\n'); // every entry (poll options), one per line
        } else {
          shown = formatValue(d, v, ctx, look);
        }
        const mono = d.type === 'text' && /Id$/.test(d.field);
        const wrap = (d.type === 'text' && !mono) || d.type === 'stringList';
        rows.push(h('div', { class: `cl-info-row${wrap ? ' is-wide' : ''}` },
          h('dt', {}, d.label), h('dd', { class: `${mono ? 'is-mono' : ''}${wrap ? ' is-wrap' : ''}`.trim() || null }, shown)));
      }
      const actions = c.actions.filter((a) => actionApplies(a, current)).map((a) => {
        const btn = h('button', {
          type: 'button',
          class: `btn btn-sm ${a.danger ? 'cl-btn-danger-soft' : 'btn-secondary'}`,
          title: a.description || null,
        }, a.id === 'publish' ? uiIcon('send') : null, a.label);
        btn.addEventListener('click', () => runAction(a, current, btn, { fromEditor: ctl }));
        return btn;
      });
      if (!rows.length && !actions.length) return null;
      const picture = typeof current[c.avatarField] === 'string' ? current[c.avatarField] : null;
      const who = c.avatarField
        ? h('div', { class: 'cl-info-who' },
          avatar(itemTitle(current), picture, { size: 'md', round: true }),
          h('span', { class: 'cl-info-name' }, itemTitle(current)))
        : null;
      return h('section', { class: 'cl-info', 'aria-label': 'Details' },
        who,
        rows.length ? h('dl', { class: 'cl-info-list' }, rows) : null,
        actions.length ? h('div', { class: 'cl-info-actions' }, actions) : null);
    }

    let infoEl = null;
    function build() {
      for (const f of forms) {
        try {
          f.form.destroy();
        } catch { /* ignore */ }
      }
      forms = [];
      mainForm = null;
      const parts = [];
      infoEl = infoSection();
      if (infoEl) parts.push(infoEl);
      const detailsView = detailsOnly();
      const lockedFields = mode === 'edit' && !detailsView ? c.fields.filter((f) => c.createOnly.includes(f.field)) : [];
      const editable = detailsView ? [] : (mode === 'edit' ? c.fields.filter((f) => !c.createOnly.includes(f.field)) : c.fields);
      const valueOf = (f) => (mode === 'edit' && current && current[f.field] !== undefined
        ? clone(current[f.field])
        : clone(f.default ?? null));
      formSeq += 1;
      const prefix = `${idBase}-e${formSeq}`;
      try {
        if (detailsView) {
          if (!infoEl) parts.push(h('p', { class: 'cl-hint' }, icon('info'), `This ${c.singular} has no details to show.`));
        } else {
          if (lockedFields.length) {
            const locked = createForm({
              fields: lockedFields,
              values: Object.fromEntries(lockedFields.map((f) => [f.field, valueOf(f)])),
              ctx,
              disabled: true,
              idPrefix: `${prefix}-locked`,
            });
            forms.push({ form: locked, names: new Set(lockedFields.map((f) => f.field)) });
            parts.push(h('div', { class: 'cl-locked' }, locked.el,
              h('p', { class: 'cl-hint' }, icon('lock'),
                `${lockedFields.map((f) => f.label).join(', ')} cannot be changed after creation.`)));
          }
          mainForm = createForm({
            fields: editable,
            values: Object.fromEntries(editable.map((f) => [f.field, valueOf(f)])),
            ctx,
            idPrefix: prefix,
            onChange: () => {
              setStatus(status, '');
              paintDirty();
            },
          });
          forms.push({ form: mainForm, names: new Set(editable.map((f) => f.field)) });
          parts.push(h('div', { class: 'cl-form' }, mainForm.el));
        }
      } catch {
        parts.push(h('p', { class: 'cl-dlg-status' }, 'This form could not be displayed. Reload the page and try again.'));
      }
      modal.body.replaceChildren(...parts);
      baseValues = valuesNow();
      baseline = canonical(baseValues);

      saveBtn.replaceChildren(icon(mode === 'create' ? 'plus' : 'check'), mode === 'create' ? `Create ${c.singular}` : 'Save changes');
      const canSave = mode === 'create' || c.canEdit;
      // (replaceChildren turns a null argument into the text "null": leave the missing buttons out.)
      modal.foot.replaceChildren(...[
        mode === 'edit' && c.canDelete ? deleteBtn : null,
        h('span', { class: 'cl-foot-spacer' }),
        status,
        dirtyNote,
        cancelBtn,
        canSave && mainForm ? saveBtn : null,
      ].filter(Boolean));
      cancelBtn.textContent = canSave ? 'Cancel' : 'Close';
      paintDirty();
    }

    function finish() {
      modals.delete(modal);
      for (const f of forms) {
        try {
          f.form.destroy();
        } catch { /* ignore */ }
      }
      editor = null;
      const focusId = current?.id ?? null;
      modal.close({ restoreFocus: false });
      if (focusId) focusRow(focusId);
      else if (createBtn.isConnected && !createBtn.disabled) createBtn.focus();
    }

    async function requestClose() {
      if (saving) return;
      if (isDirty()) {
        const discard = await confirmModal({
          title: 'Discard unsaved changes?',
          message: mode === 'create' ? `This ${c.singular} has not been created yet.` : 'Your changes have not been saved.',
          confirmLabel: 'Discard',
          cancelLabel: 'Keep editing',
          danger: true,
        });
        if (!discard) return;
      }
      finish();
    }

    // Returns the saved item, or null when the save failed.
    async function save({ keepOpen = false } = {}) {
      if (saving || !mainForm) return null;
      setStatus(status, '');
      for (const f of forms) {
        try {
          f.form.setErrors([]);
        } catch { /* ignore */ }
      }
      const values = valuesNow();
      let request;
      if (mode === 'create') {
        request = { method: 'POST', url: base, data: values };
      } else {
        const data = {};
        for (const [k, v] of Object.entries(values)) if (canonical(v) !== canonical(baseValues[k])) data[k] = v;
        if (!Object.keys(data).length) {
          if (!keepOpen) finish();
          return current;
        }
        request = { method: 'PATCH', url: `${base}/${encodeURIComponent(current.id)}`, data };
      }
      saving = true;
      busy(saveBtn, true, { announce });
      cancelBtn.disabled = true;
      try {
        const res = await api(request.url, { method: request.method, body: { data: request.data } });
        const created = mode === 'create';
        const saved = isObj(res?.item) ? res.item : (created ? null : current);
        if (saved) upsert(saved, { prepend: created });
        else load(); // the bot did the work and returned no item: show what it made
        current = saved ?? current;
        toastResult(res, created
          ? (saved ? `Created "${itemTitle(saved)}".` : `${c.singular[0].toUpperCase()}${c.singular.slice(1)} created.`)
          : 'Changes saved.');
        if (created && saved && c.canEdit && c.actions.length && !modal.closed) {
          // Stay open in edit mode, so the new item can be posted or used right away.
          mode = 'edit';
          modal.setTitle(`Edit ${c.singular}`, itemTitle(saved));
          build();
          queueFocus(modal.body.querySelector('.cl-info-actions button') ?? saveBtn);
        } else if (keepOpen) {
          baseValues = valuesNow();
          baseline = canonical(baseValues);
          refreshInfo();
        } else {
          finish();
        }
        return saved;
      } catch (err) {
        const info = writeError(err, c, limit);
        if (info.where === 'reload') {
          location.reload();
          return null;
        }
        if (info.where === 'field' && routeFieldError(forms, info.path, info.message)) {
          setStatus(status, 'Please fix the highlighted field.');
        } else {
          setStatus(status, info.message);
        }
        if (info.gone) load();
        return null;
      } finally {
        saving = false;
        busy(saveBtn, false);
        cancelBtn.disabled = false;
        paintDirty();
      }
    }

    function refreshInfo() {
      const next = infoSection();
      if (infoEl && next) infoEl.replaceWith(next);
      else if (infoEl) infoEl.remove();
      else if (next) modal.body.prepend(next);
      infoEl = next;
    }

    const ctl = {
      isDirty,
      save,
      itemId: () => current?.id ?? null,
      updateItem(next) {
        current = next;
        modal.setTitle(`${c.canEdit ? 'Edit' : 'View'} ${c.singular}`, itemTitle(next));
        refreshInfo();
      },
      showError: (message) => setStatus(status, message),
      close: finish,
    };
    editor = ctl;
    build();
    queueFocus(modal.body.querySelector('input:not([disabled]), textarea:not([disabled]), select:not([disabled])')
      ?? modal.body.querySelector('.cl-info-actions button') ?? (mainForm ? saveBtn : cancelBtn));
  }

  /* ---- start ---- */

  render();
  load();

  return {
    refresh: () => load(),
    destroy() {
      destroyed = true;
      loadSeq += 1;
      clearTimeout(searchTimer);
      closeMenus();
      for (const m of [...modals]) m.close({ restoreFocus: false });
      modals.clear();
      editor = null;
      root.remove();
    },
  };
}
