// Per-server record lists of the web dashboard (saved embeds, custom commands, level rewards, ...):
//   mountCollection(container, { collection, ctx }) -> { destroy(), refresh() }
// A list with a count and an empty state, an editor dialog built with createForm (forms.js), actions with their own
// small forms, and delete with a confirmation. API: /api/guilds/:id/records/:key (dashboard contract, section 5).
//
// Import-safe in plain Node: nothing touches document, window or storage until a function is called.

import { h, icon, api, ApiError, toast, describeError, canonical, slug, safeHref, TOAST_ERROR_MS } from './lib.js';
import { createForm } from './forms.js';
import { templateSummary, sampleText, uiIcon, colorHex } from './embed-editor.js';

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
// buttons; without a dialog, in an error toast). Discord's refusals (502, e.g. a missing permission) are 'status'.
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
      message: msg || (err.code === 'limit'
        ? `You have reached the limit of ${limit} ${c.label.toLowerCase()}.`
        : 'That conflicts with an existing entry.'),
    };
    case 502: return { where: 'status', message: msg || "Discord refused the request. Check the bot's permissions and try again." };
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
    limit: Number.isInteger(c.limit) ? c.limit : null,
    titleField: String(c.titleField ?? 'id'),
    subtitleField: c.subtitleField ? String(c.subtitleField) : null,
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
  const rowAction = c.actions.find((a) => !a.danger) ?? null;
  const isPosted = (item) => Boolean(item?.channelId && item?.messageId);
  // Actions on the posted copy (update it, delete it) mean nothing for a draft.
  const actionApplies = (a, item) => !(publishable && !isPosted(item) && ['refresh', 'unpublish', 'update'].includes(a.id));
  let items = [];
  let limit = c.limit;
  let phase = 'loading'; // loading | ready | error
  let loadErr = null;
  let destroyed = false;
  let loadSeq = 0;
  let formSeq = 0;
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
  const root = h(
    'div',
    { class: 'cl', 'data-collection': c.key },
    h('div', { class: 'cl-toolbar' },
      countEl, limitNote, h('span', { class: 'cl-spacer' }), reloadBtn, c.canCreate ? createBtn : null),
    bodyEl,
  );
  container.append(root);
  reloadBtn.addEventListener('click', () => load());
  createBtn.addEventListener('click', () => openEditor(null));

  const atLimit = () => Number.isInteger(limit) && items.length >= limit;

  function paintToolbar() {
    const counted = Number.isInteger(limit) ? `${items.length} / ${limit}` : String(items.length);
    countEl.textContent = phase === 'ready' ? counted : '';
    const full = phase === 'ready' && atLimit();
    createBtn.disabled = phase !== 'ready' || full;
    limitNote.hidden = !full;
    limitNote.textContent = full ? 'Limit reached. Delete one to add another.' : '';
    reloadBtn.disabled = phase === 'loading';
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

  function rowFor(item, look) {
    const title = itemTitle(item);
    const titleId = `${idBase}-r-${slug(item.id)}`;
    const sub = statusNode(item, look);
    const badges = badgesFor(item);
    const main = h(
      'button',
      { type: 'button', class: 'cl-row-main', 'aria-labelledby': titleId, title: c.canEdit ? `Edit ${title}` : `View ${title}` },
      miniPreview(item),
      h('span', { class: 'cl-row-text' },
        h('span', { class: 'cl-row-title', id: titleId }, title),
        sub || badges.length ? h('span', { class: 'cl-row-sub' }, badges, sub) : null),
      h('span', { class: 'cl-row-chevron', 'aria-hidden': 'true' }, icon('forward')),
    );
    main.addEventListener('click', () => openEditor(item));
    const actionBtn = rowAction
      ? h(
        'button',
        {
          type: 'button', class: 'btn btn-secondary btn-sm cl-row-action', 'aria-describedby': titleId, title: rowAction.description || null,
        },
        rowAction.id === 'publish' ? uiIcon('send') : null,
        rowAction.label,
      )
      : null;
    actionBtn?.addEventListener('click', () => runAction(rowAction, item, actionBtn));
    return h('li', { class: 'cl-row', 'data-id': item.id }, main,
      actionBtn ? h('div', { class: 'cl-row-actions' }, actionBtn) : null);
  }

  function skeleton() {
    return h('ul', { class: 'cl-list', 'aria-hidden': 'true' }, [0, 1, 2].map(() => h('li', { class: 'cl-row is-skeleton' },
      h('span', { class: 'skeleton cl-skel-mini' }),
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

  function render({ focusId = null } = {}) {
    if (destroyed) return;
    paintToolbar();
    root.setAttribute('aria-busy', phase === 'loading' ? 'true' : 'false');
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
      bodyEl.replaceChildren(emptyState());
      return;
    }
    const look = lookups(ctx);
    listEl.replaceChildren(...items.map((item) => rowFor(item, look)));
    bodyEl.replaceChildren(listEl);
    if (focusId !== null) listEl.querySelector(`[data-id="${CSS.escape(focusId)}"] .cl-row-main`)?.focus();
  }

  async function load() {
    const seq = ++loadSeq;
    phase = items.length ? phase : 'loading';
    if (!items.length) render();
    reloadBtn.disabled = true;
    try {
      const data = await api(base);
      if (destroyed || seq !== loadSeq) return;
      items = Array.isArray(data?.items) ? data.items.filter((x) => isObj(x) && typeof x.id === 'string') : [];
      if (Number.isInteger(data?.limit)) limit = data.limit;
      phase = 'ready';
      loadErr = null;
    } catch (err) {
      if (destroyed || seq !== loadSeq) return;
      if (err instanceof ApiError && err.status === 401) {
        location.reload();
        return;
      }
      if (phase !== 'ready') {
        phase = 'error';
        loadErr = err;
      } else {
        notify(`Could not reload ${c.label.toLowerCase()}: ${describeError(err).message}`, { kind: 'error', ms: TOAST_ERROR_MS });
      }
    }
    render({ focusId: focusedRowId() });
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

  async function postAction(action, item, data) {
    const url = `${base}/${encodeURIComponent(item.id)}/actions/${encodeURIComponent(action.id)}`;
    const res = await api(url, { method: 'POST', body: { data } });
    const updated = isObj(res?.item) ? res.item : item;
    upsert(updated);
    if (editor && editor.itemId() === updated.id) editor.updateItem(updated);
    notify(res?.message || `${action.label}: done.`, { kind: 'success' });
    return updated;
  }

  function busy(btn, on) {
    if (!btn) return;
    btn.disabled = on;
    btn.classList.toggle('is-busy', on);
    if (on) btn.setAttribute('aria-busy', 'true');
    else btn.removeAttribute('aria-busy');
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
      busy(trigger, true);
      try {
        await postAction(action, target, {});
      } catch (err) {
        const info = writeError(err, c, limit);
        if (info.where === 'reload') location.reload();
        else notify(info.message, { kind: 'error', ms: TOAST_ERROR_MS });
        if (info.gone) load();
      } finally {
        if (trigger?.isConnected) busy(trigger, false);
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
      busy(run, true);
      try {
        await postAction(action, item, form.getValues());
        close();
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
        if (run.isConnected) busy(run, false);
      }
    });
    queueFocus(modal.body.querySelector('input, select, textarea, button, [tabindex]') ?? run);
  }

  /* ---- delete ---- */

  async function deleteItem(item, trigger) {
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
    busy(trigger, true);
    try {
      await api(`${base}/${encodeURIComponent(item.id)}`, { method: 'DELETE' });
      dropItem(item.id);
      notify(`Deleted "${title}".`, { kind: 'success' });
      return true;
    } catch (err) {
      const info = writeError(err, c, limit);
      if (info.where === 'reload') location.reload();
      else notify(info.message, { kind: 'error', ms: TOAST_ERROR_MS });
      if (info.gone) {
        dropItem(item.id);
        return true;
      }
      return false;
    } finally {
      if (trigger?.isConnected) busy(trigger, false);
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
      const gone = await deleteItem(current, deleteBtn);
      if (gone) finish();
    });

    const valuesNow = () => (mainForm ? mainForm.getValues() : {});
    const isDirty = () => Boolean(mainForm) && canonical(valuesNow()) !== baseline;
    function paintDirty() {
      const dirty = isDirty();
      dirtyNote.hidden = !dirty || mode === 'create';
      if (mode === 'edit') saveBtn.disabled = saving || !dirty;
    }

    function infoSection() {
      if (mode !== 'edit' || !current) return null;
      const look = lookups(ctx);
      const rows = [];
      for (const d of c.readOnly) {
        const v = current[d.field];
        if (v === null || v === undefined || v === '') continue;
        let shown;
        if (d.type === 'url') {
          const href = safeHref(String(v));
          const text = /discord\.com\/channels\//.test(String(v)) ? 'Open in Discord' : formatValue(d, v, ctx, look);
          shown = href ? h('a', { href, target: '_blank', rel: 'noopener noreferrer' }, text, uiIcon('external')) : '—';
        } else if (d.field === 'status' && publishable) {
          shown = statusNode(current, look);
        } else {
          shown = formatValue(d, v, ctx, look);
        }
        const mono = d.type === 'text' && /Id$/.test(d.field);
        rows.push(h('div', { class: 'cl-info-row' }, h('dt', {}, d.label), h('dd', { class: mono ? 'is-mono' : null }, shown)));
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
      return h('section', { class: 'cl-info', 'aria-label': 'Details' },
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
      const lockedFields = mode === 'edit' ? c.fields.filter((f) => c.createOnly.includes(f.field)) : [];
      const editable = mode === 'edit' ? c.fields.filter((f) => !c.createOnly.includes(f.field)) : c.fields;
      const valueOf = (f) => (mode === 'edit' && current && current[f.field] !== undefined
        ? clone(current[f.field])
        : clone(f.default ?? null));
      formSeq += 1;
      const prefix = `${idBase}-e${formSeq}`;
      try {
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
          disabled: mode === 'edit' && !c.canEdit,
          idPrefix: prefix,
          onChange: () => {
            setStatus(status, '');
            paintDirty();
          },
        });
        forms.push({ form: mainForm, names: new Set(editable.map((f) => f.field)) });
        parts.push(h('div', { class: 'cl-form' }, mainForm.el));
      } catch {
        parts.push(h('p', { class: 'cl-dlg-status' }, 'This form could not be displayed. Reload the page and try again.'));
      }
      modal.body.replaceChildren(...parts);
      baseValues = valuesNow();
      baseline = canonical(baseValues);

      saveBtn.replaceChildren(icon(mode === 'create' ? 'plus' : 'check'), mode === 'create' ? `Create ${c.singular}` : 'Save changes');
      const canSave = mode === 'create' || c.canEdit;
      modal.foot.replaceChildren(
        mode === 'edit' && c.canDelete ? deleteBtn : null,
        h('span', { class: 'cl-foot-spacer' }),
        status,
        dirtyNote,
        cancelBtn,
        canSave && mainForm ? saveBtn : null,
      );
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
      busy(saveBtn, true);
      cancelBtn.disabled = true;
      try {
        const res = await api(request.url, { method: request.method, body: { data: request.data } });
        const saved = isObj(res?.item) ? res.item : current;
        const created = mode === 'create';
        upsert(saved, { prepend: created });
        current = saved;
        notify(created ? `Created "${itemTitle(saved)}".` : 'Changes saved.', { kind: 'success' });
        if (created && c.actions.length && !modal.closed) {
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
        if (saveBtn.isConnected) busy(saveBtn, false);
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
      close: finish,
    };
    editor = ctl;
    build();
    queueFocus(modal.body.querySelector('input:not([disabled]), textarea:not([disabled]), select:not([disabled])') ?? saveBtn);
  }

  /* ---- start ---- */

  render();
  load();

  return {
    refresh: () => load(),
    destroy() {
      destroyed = true;
      loadSeq += 1;
      for (const m of [...modals]) m.close({ restoreFocus: false });
      modals.clear();
      editor = null;
      root.remove();
    },
  };
}
