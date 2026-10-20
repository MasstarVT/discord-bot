// Form controls for the dashboard: one control per field-descriptor type (src/dashboard/schema.js), a form that
// combines them, and the combobox / popover machinery behind the channel, role, member, time zone and emoji pickers.
// Values go in and come out in the wire shapes of the descriptors, so getValues() can be PATCHed as it is.
//
// Nothing here touches the DOM, window or storage at import time (the module is import-checked in plain Node);
// every function does its work only when called. Styles live in forms.css (classes are prefixed `fc-`).
import { h, icon, slug, api as request, avatar, createSwitch, paintSwitch } from './lib.js';
import { createTemplateEditor } from './embed-editor.js';

/* ------------------------------------------------------------------ */
/* Small helpers                                                        */
/* ------------------------------------------------------------------ */

const SVG_NS = 'http://www.w3.org/2000/svg';
const SNOWFLAKE_RE = /^\d{17,20}$/;
const HEX_RE = /^#?([0-9a-f]{6})$/i;
const HEX3_RE = /^#?([0-9a-f]{3})$/i;
const CUSTOM_EMOJI_RE = /^<(a?):(\w{2,32}):(\d{17,20})>$/;
const EMOJI_HINT_RE = /\p{Extended_Pictographic}|\p{Emoji_Presentation}|[\u{1F1E6}-\u{1F1FF}]|^[0-9#*]️?⃣$/u;
const EMOJI_CDN = 'https://cdn.discordapp.com/emojis';
const BIG_LIST = 100;

// Icons the shared set in lib.js does not have (24x24 stroke paths, sub-paths joined with "|").
const LOCAL_ICONS = {
  up: 'M18 15l-6-6-6 6',
  down: 'M6 9l6 6 6-6',
  trash: 'M3 6h18|M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2|M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6|M10 11v6|M14 11v6',
  smile: 'M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20z|M8 14s1.5 2 4 2 4-2 4-2|M9 9h.01|M15 9h.01',
};

function localIcon(name) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  for (const d of (LOCAL_ICONS[name] || '').split('|')) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

const clone = (v) => (v === undefined ? undefined : structuredClone(v));
const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const isSnowflake = (v) => typeof v === 'string' && SNOWFLAKE_RE.test(v);
const pad2 = (n) => String(n).padStart(2, '0');
const plural = (n, one, many = `${one}s`) => (n === 1 ? one : many);

let keySeq = 0;
const nextKey = () => (keySeq += 1);

const prefersReducedMotion = () => typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

/** The value a freshly added list item (or a missing child) starts with. */
function defaultFor(desc) {
  if (desc.default !== undefined) return clone(desc.default);
  switch (desc.type) {
    case 'boolean': return false;
    case 'int':
    case 'float': return desc.nullable ? null : (Number.isFinite(desc.min) ? desc.min : 0);
    case 'text':
    case 'url': return desc.nullable ? null : '';
    case 'enum': return desc.nullable ? null : (desc.options?.[0]?.value ?? null);
    case 'multiEnum':
    case 'channels':
    case 'roles':
    case 'stringList':
    case 'objectList': return [];
    case 'object': return Object.fromEntries((desc.fields ?? []).map((f) => [f.field, defaultFor(f)]));
    case 'template': return { content: '', embed: null, buttons: [] };
    case 'timezone': return 'UTC';
    case 'time': return '00:00';
    default: return null;
  }
}

function normalizeIds(value) {
  return Array.isArray(value) ? [...new Set(value.filter((v) => typeof v === 'string' && v))] : [];
}

/** Inserts text into an input/textarea at the caret (or the remembered selection) and fires `input`. */
function insertAtCaret(input, text, selection) {
  const start = selection?.start ?? input.selectionStart ?? input.value.length;
  const end = selection?.end ?? input.selectionEnd ?? start;
  input.focus({ preventScroll: true });
  input.setRangeText(text, start, end, 'end');
  input.dispatchEvent(new Event('input', { bubbles: true }));
}

/* ------------------------------------------------------------------ */
/* Popover and combobox engine                                          */
/* ------------------------------------------------------------------ */

/**
 * A floating panel anchored under (or above) an element. It uses the top layer when the browser has the Popover
 * API, so it is never clipped by an overflow container, and always fixed-positions itself inside the viewport.
 * `root` is the element whose subtree counts as "inside" for outside-click dismissal; `align: 'end'` lines the
 * panel's right edge up with the anchor's (a menu opened from a button at the end of a row).
 */
export function createPopover(anchor, { root, className = '', onDismiss, minWidth = 0, maxHeight = 360, align = 'start' } = {}) {
  const el = h('div', { class: `fc-pop ${className}`.trim(), hidden: true });
  const topLayer = typeof el.showPopover === 'function';
  if (topLayer) el.setAttribute('popover', 'manual');
  let open = false;
  let frame = 0;

  function place() {
    if (!open) return;
    const rect = anchor.getBoundingClientRect();
    const vv = window.visualViewport;
    const vw = document.documentElement.clientWidth;
    const bottomEdge = vv ? vv.offsetTop + vv.height : window.innerHeight;
    const gap = 4;
    const edge = 8;
    const width = Math.min(Math.max(rect.width, minWidth), vw - edge * 2);
    const wanted = align === 'end' ? rect.right - width : rect.left;
    const left = Math.max(edge, Math.min(wanted, vw - width - edge));
    const below = bottomEdge - rect.bottom - gap - edge;
    const above = rect.top - gap - edge;
    // Measure the natural height (the scrolling list inside shrinks to max-height otherwise).
    el.style.removeProperty('max-height');
    const flip = below < Math.min(el.offsetHeight, 240) && above > below;
    el.style.setProperty('width', `${width}px`);
    el.style.setProperty('left', `${left}px`);
    el.style.setProperty('max-height', `${Math.max(120, Math.min(maxHeight, flip ? above : below))}px`);
    if (flip) {
      el.style.setProperty('top', 'auto');
      el.style.setProperty('bottom', `${window.innerHeight - rect.top + gap}px`);
    } else {
      el.style.setProperty('bottom', 'auto');
      el.style.setProperty('top', `${rect.bottom + gap}px`);
    }
  }

  const schedule = () => {
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      place();
    });
  };
  const onScroll = (event) => {
    if (!el.contains(event.target)) schedule();
  };
  const onPointerDown = (event) => {
    if (!root.contains(event.target)) onDismiss?.();
  };

  return {
    el,
    get isOpen() { return open; },
    place,
    open() {
      if (open) return;
      open = true;
      el.hidden = false;
      if (topLayer) {
        try { el.showPopover(); } catch { /* not connected yet, or already showing */ }
      }
      place();
      window.addEventListener('resize', schedule);
      window.addEventListener('scroll', onScroll, true);
      window.visualViewport?.addEventListener('resize', schedule);
      document.addEventListener('pointerdown', onPointerDown, true);
    },
    close() {
      if (!open) return;
      open = false;
      if (topLayer) {
        try { el.hidePopover(); } catch { /* already hidden */ }
      }
      el.hidden = true;
      if (frame) cancelAnimationFrame(frame);
      frame = 0;
      window.removeEventListener('resize', schedule);
      window.removeEventListener('scroll', onScroll, true);
      window.visualViewport?.removeEventListener('resize', schedule);
      document.removeEventListener('pointerdown', onPointerDown, true);
    },
  };
}

/**
 * ARIA 1.2 combobox over a text input: a filterable listbox of grouped options in a popover.
 * `getGroups(query)` returns `{ groups: [{ label?, options: [data] }], note?, status? }`; each `data` may carry `disabled`,
 * and `status` replaces the text announced to screen readers ("Searching…").
 * Keyboard: Arrow keys move, Enter picks, Escape closes (and is not passed on to a surrounding dialog), Tab leaves.
 */
function createCombobox({
  id, input, anchor, root, label, multi = false, getGroups, renderOption, isSelected, onPick, onOpenChange,
  footer = null, maxRender = 100, emptyText = 'No matches',
}) {
  const list = h('div', {
    class: 'fc-list', role: 'listbox', id: `${id}-list`, 'aria-label': label, 'aria-multiselectable': multi ? 'true' : null,
  });
  const status = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
  const pop = createPopover(anchor, { root, className: 'fc-pop-list', onDismiss: () => close() });
  pop.el.append(list);
  if (footer) pop.el.append(h('div', { class: 'fc-pop-foot' }, footer));
  root.append(pop.el, status);
  // Keep the focus in the input while the pointer works the list.
  pop.el.addEventListener('mousedown', (event) => event.preventDefault());

  let query = '';
  let options = [];
  let active = -1;
  let disabled = false;

  function setActive(index, { scroll = true } = {}) {
    if (options[active]) options[active].el.classList.remove('is-active');
    active = index;
    const opt = options[index];
    if (!opt) {
      input.removeAttribute('aria-activedescendant');
      return;
    }
    opt.el.classList.add('is-active');
    input.setAttribute('aria-activedescendant', opt.el.id);
    if (scroll) {
      const top = opt.el.offsetTop;
      const bottom = top + opt.el.offsetHeight;
      if (top < list.scrollTop) list.scrollTop = top;
      else if (bottom > list.scrollTop + list.clientHeight) list.scrollTop = bottom - list.clientHeight;
    }
  }

  function nextEnabled(from, dir) {
    const n = options.length;
    let i = from;
    for (let k = 0; k < n; k += 1) {
      i = i < 0 ? (dir > 0 ? 0 : n - 1) : (i + dir + n) % n;
      if (!options[i].data.disabled) return i;
    }
    return -1;
  }

  function pick(index) {
    const opt = options[index];
    if (opt && !opt.data.disabled) onPick(opt.data);
  }

  function render() {
    const { groups, note, status: announced } = getGroups(query);
    const nodes = [];
    let truncated = false;
    let groupIndex = 0;
    options = [];
    for (const group of groups) {
      if (!group.options.length) continue;
      const items = [];
      for (const data of group.options) {
        if (options.length >= maxRender) {
          truncated = true;
          break;
        }
        const index = options.length;
        const el = h('div', {
          class: `fc-opt${data.disabled ? ' is-disabled' : ''}`,
          role: 'option',
          id: `${id}-o${index}`,
          'aria-selected': isSelected(data) ? 'true' : 'false',
          'aria-disabled': data.disabled ? 'true' : null,
          onclick: () => pick(index),
          onmousemove: () => {
            if (active !== index && !data.disabled) setActive(index, { scroll: false });
          },
        }, renderOption(data));
        options.push({ el, data });
        items.push(el);
      }
      if (group.label) {
        const gid = `${id}-g${groupIndex}`;
        groupIndex += 1;
        nodes.push(h('div', { class: 'fc-group', role: 'group', 'aria-labelledby': gid },
          h('div', { class: 'fc-group-label', id: gid }, group.label), items));
      } else {
        nodes.push(...items);
      }
      if (truncated) break;
    }
    if (!options.length) nodes.push(h('div', { class: 'fc-note', role: 'presentation' }, note || emptyText));
    else if (note) nodes.push(h('div', { class: 'fc-note', role: 'presentation' }, note));
    if (truncated) {
      const more = `Showing the first ${maxRender}. Keep typing to narrow the list.`;
      nodes.push(h('div', { class: 'fc-note', role: 'presentation' }, more));
    }
    list.replaceChildren(...nodes);
    status.textContent = announced ?? (options.length ? `${options.length} ${plural(options.length, 'result')}` : 'No results');
    const selected = options.findIndex((o) => isSelected(o.data));
    setActive(query ? nextEnabled(-1, 1) : selected, { scroll: true });
    pop.place();
  }

  function open() {
    if (disabled || pop.isOpen) return;
    input.setAttribute('aria-expanded', 'true');
    pop.open();
    render();
    onOpenChange?.(true);
  }

  function close() {
    if (!pop.isOpen) return;
    pop.close();
    input.setAttribute('aria-expanded', 'false');
    input.removeAttribute('aria-activedescendant');
    active = -1;
    query = '';
    onOpenChange?.(false);
  }

  input.addEventListener('input', () => {
    query = input.value;
    if (pop.isOpen) render();
    else open();
  });
  input.addEventListener('keydown', (event) => {
    if (event.isComposing) return;
    switch (event.key) {
      case 'ArrowDown':
      case 'ArrowUp': {
        event.preventDefault();
        const dir = event.key === 'ArrowDown' ? 1 : -1;
        if (!pop.isOpen) {
          open();
          if (active < 0) setActive(nextEnabled(-1, dir));
        } else {
          setActive(nextEnabled(active, dir));
        }
        break;
      }
      case 'Enter':
        if (pop.isOpen && active >= 0) {
          event.preventDefault();
          pick(active);
        }
        break;
      case 'Escape':
        if (pop.isOpen) {
          event.preventDefault();
          event.stopPropagation();
          close();
        }
        break;
      case 'Tab':
        close();
        break;
      default:
        break;
    }
  });
  root.addEventListener('focusout', (event) => {
    if (!event.relatedTarget || !root.contains(event.relatedTarget)) close();
  });

  return {
    open,
    close,
    get isOpen() { return pop.isOpen; },
    get query() { return query; },
    refresh() { if (pop.isOpen) render(); },
    setQuery(q) {
      query = q;
      if (pop.isOpen) render();
    },
    setDisabled(value) {
      disabled = value;
      if (value) close();
    },
    destroy() { close(); },
  };
}

/* ------------------------------------------------------------------ */
/* Resources: channels, roles, time zones                               */
/* ------------------------------------------------------------------ */

const GLYPHS = {
  text: '#', voice: '🔊', announcement: '📢', stage: '🎙', forum: '💬', category: '📁', media: '🖼',
};
const KIND_NAMES = {
  text: 'text channel', voice: 'voice channel', announcement: 'announcement channel', stage: 'stage channel',
  forum: 'forum channel', category: 'category', media: 'media channel',
};

const lookups = new WeakMap();

/** Id maps for a resources payload, built once per payload. */
function lookup(res) {
  let found = lookups.get(res);
  if (!found) {
    found = {
      channels: new Map((res.channels ?? []).map((c) => [c.id, c])),
      roles: new Map((res.roles ?? []).map((r) => [r.id, r])),
    };
    lookups.set(res, found);
  }
  return found;
}

const safeColor = (c) => (typeof c === 'string' && /^#[0-9a-f]{6}$/i.test(c) ? c : null);

/** The small marker in front of an option or chip: a channel glyph or a role colour dot. */
function decoNode(data) {
  if (data.glyph) return h('span', { class: 'fc-glyph', 'aria-hidden': 'true' }, data.glyph);
  if (data.dot) {
    const dot = h('span', { class: 'fc-dot', 'aria-hidden': 'true' });
    const color = safeColor(data.color);
    if (color) dot.style.setProperty('--role', color);
    else dot.classList.add('is-plain');
    return dot;
  }
  return null;
}

function idEntryGroups(noun, query) {
  const q = query.trim();
  if (isSnowflake(q)) return { groups: [{ options: [{ value: q, label: `Use ${noun} ID ${q}` }] }] };
  return { groups: [], note: `The server's ${noun}s could not be loaded. Paste a ${noun} ID to add it.` };
}

function channelConfig(desc, st, multi) {
  const kinds = new Set(desc.channelTypes ?? Object.keys(GLYPHS));
  return {
    noun: 'channel',
    deleted: 'Deleted channel',
    placeholder: multi ? 'Add a channel…' : 'Select a channel',
    needsResources: true,
    describe(id) {
      const res = st.getRes();
      if (!res || !Array.isArray(res.channels)) return { label: id, unknown: true };
      const c = lookup(res).channels.get(id);
      if (!c) return { label: 'Deleted channel', missing: true };
      return { label: c.name, glyph: GLYPHS[c.kind] ?? '#', kind: KIND_NAMES[c.kind] };
    },
    groups(query) {
      const res = st.getRes();
      if (!res || !Array.isArray(res.channels)) return idEntryGroups('channel', query);
      const q = query.trim().toLowerCase();
      const byId = lookup(res).channels;
      const groups = new Map();
      for (const c of res.channels) {
        if (!kinds.has(c.kind)) continue;
        const parent = c.kind === 'category' ? null : byId.get(c.parentId);
        if (q && !c.name.toLowerCase().includes(q) && !parent?.name.toLowerCase().includes(q)) continue;
        const key = c.kind === 'category' ? '#categories' : (parent?.id ?? '');
        if (!groups.has(key)) {
          groups.set(key, { label: key === '#categories' ? 'Categories' : (parent?.name ?? null), options: [] });
        }
        groups.get(key).options.push({
          value: c.id, label: c.name, glyph: GLYPHS[c.kind] ?? '#', kind: KIND_NAMES[c.kind], hint: c.nsfw ? 'NSFW' : null,
        });
      }
      const out = [...groups.values()];
      if (out.length > 1 && out[0].label === null) out[0].label = 'No category';
      return { groups: out };
    },
  };
}

function roleConfig(desc, st, multi) {
  return {
    noun: 'role',
    deleted: 'Deleted role',
    placeholder: multi ? 'Add a role…' : 'Select a role',
    needsResources: true,
    describe(id) {
      const res = st.getRes();
      if (!res || !Array.isArray(res.roles)) return { label: id, unknown: true };
      const r = lookup(res).roles.get(id);
      if (!r) return { label: 'Deleted role', missing: true };
      return { label: r.name, dot: true, color: r.color };
    },
    groups(query) {
      const res = st.getRes();
      if (!res || !Array.isArray(res.roles)) return idEntryGroups('role', query);
      const q = query.trim().toLowerCase();
      const options = [];
      for (const r of [...res.roles].sort((a, b) => (b.position ?? 0) - (a.position ?? 0))) {
        if (r.everyone && !desc.allowEveryone) continue;
        if (q && !r.name.toLowerCase().includes(q)) continue;
        const blocked = desc.assignable === true && (r.managed || !r.assignable);
        let hint = null;
        if (blocked) {
          if (r.managed) hint = 'Managed by an integration';
          else hint = r.everyone ? 'Cannot be given' : "Above the bot's highest role";
        }
        options.push({ value: r.id, label: r.name, dot: true, color: r.color, disabled: blocked, hint });
      }
      return { groups: [{ options }] };
    },
  };
}

let zoneCache = null;
const offsetCache = new Map();
const FALLBACK_ZONES = [
  'Africa/Cairo', 'Africa/Johannesburg', 'Africa/Lagos', 'America/Anchorage', 'America/Argentina/Buenos_Aires',
  'America/Bogota', 'America/Chicago', 'America/Denver', 'America/Halifax', 'America/Los_Angeles', 'America/Mexico_City',
  'America/New_York', 'America/Sao_Paulo', 'America/Toronto', 'Asia/Bangkok', 'Asia/Dhaka', 'Asia/Dubai', 'Asia/Hong_Kong',
  'Asia/Jakarta', 'Asia/Karachi', 'Asia/Kolkata', 'Asia/Manila', 'Asia/Seoul', 'Asia/Shanghai', 'Asia/Singapore',
  'Asia/Tehran', 'Asia/Tokyo', 'Atlantic/Reykjavik', 'Australia/Adelaide', 'Australia/Brisbane', 'Australia/Perth',
  'Australia/Sydney', 'Europe/Amsterdam', 'Europe/Athens', 'Europe/Berlin', 'Europe/Istanbul', 'Europe/Lisbon',
  'Europe/London', 'Europe/Madrid', 'Europe/Moscow', 'Europe/Paris', 'Europe/Rome', 'Europe/Stockholm', 'Europe/Warsaw',
  'Pacific/Auckland', 'Pacific/Honolulu',
];

function zoneList() {
  if (zoneCache) return zoneCache;
  let zones = [];
  try {
    if (typeof Intl !== 'undefined' && typeof Intl.supportedValuesOf === 'function') zones = Intl.supportedValuesOf('timeZone');
  } catch { /* fall back to the short list */ }
  if (!zones.length) zones = FALLBACK_ZONES;
  zoneCache = ['UTC', ...zones.filter((z) => z !== 'UTC')];
  return zoneCache;
}

function zoneOffset(zone) {
  if (offsetCache.has(zone)) return offsetCache.get(zone);
  let text = '';
  try {
    const parts = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'shortOffset' }).formatToParts(new Date());
    text = parts.find((p) => p.type === 'timeZoneName')?.value ?? '';
  } catch { /* the offset is only a hint */ }
  offsetCache.set(zone, text);
  return text;
}

function timezoneConfig() {
  return {
    noun: 'time zone',
    placeholder: 'Search time zones…',
    describe: (id) => ({ label: id }),
    groups(query) {
      const q = query.trim().toLowerCase().replace(/\s+/g, '_');
      const zones = zoneList().filter((z) => !q || z.toLowerCase().includes(q));
      return { groups: [{ options: zones.map((z) => ({ value: z, label: z })) }] };
    },
    optionHint: zoneOffset,
  };
}

/* ------------------------------------------------------------------ */
/* Field shell: label, description, error, aria wiring                  */
/* ------------------------------------------------------------------ */

/**
 * Builds one form control for a descriptor. `onChange(value)` fires for user edits only (setValue is silent).
 * Template and embed descriptors are delegated to the template editor, which owns their label and messages.
 * Element ids are `${idPrefix}-${field}...`: give every form on one page its own `idPrefix`.
 * @param {object} desc a field descriptor (src/dashboard/schema.js; the browser copy from the catalog)
 * @param {{ value?: unknown, ctx?: object|null, onChange?: (value: unknown) => void, disabled?: boolean,
 *   idPrefix?: string }} [opts]
 * @returns {{ el: HTMLElement, getValue: () => unknown, setValue: (v: unknown) => void,
 *   setError: (message: string|null, subPath?: string) => void, setResources: (resources: object|null) => void,
 *   focus: () => void, destroy: () => void, setDisabled: (disabled: boolean) => void,
 *   focusPath: (subPath?: string) => void }}
 */
export function createField(desc, opts = {}) {
  const { value, ctx = null, onChange, disabled = false, idPrefix = 'f' } = opts;
  if (desc?.type === 'template' || desc?.type === 'embed') return delegateField(desc, opts);

  const builder = BUILDERS[desc?.type];
  const id = `${idPrefix}-${slug(desc?.field ?? desc?.label ?? 'field')}`;
  const labelId = `${id}-label`;
  const descId = `${id}-desc`;
  const errId = `${id}-err`;
  const initial = value === undefined ? defaultFor(desc ?? {}) : value;

  let override;
  let seenCtxResources = ctx?.resources;
  let serverError = null;
  let localError = null;
  let built = null;
  let ready = false;

  // Resources set explicitly win until the ctx hands over a different payload.
  const getRes = () => {
    if (ctx && ctx.resources !== seenCtxResources) {
      seenCtxResources = ctx.resources;
      override = undefined;
    }
    return override !== undefined ? override : (ctx?.resources ?? null);
  };

  const st = {
    id, labelId, descId, errId, desc, ctx, value: initial, getRes,
    // A live view of the ctx for nested forms: everything inherited, `resources` always the current payload.
    childCtx: () => Object.create(ctx ?? {}, { resources: { get: getRes, enumerable: true } }),
    changed() {
      if (serverError) {
        serverError = null;
        paintError();
      }
      onChange?.(built.getValue());
    },
    setLocalError(message) {
      localError = message || null;
      if (ready) paintError();
    },
    async reload() {
      if (typeof ctx?.reloadResources !== 'function') return;
      const fresh = await ctx.reloadResources();
      if (fresh) api.setResources(fresh);
    },
  };

  if (!builder) {
    built = {
      body: h('p', { class: 'fc-desc' }, `This setting type (${desc?.type}) is not supported in the dashboard yet.`),
      getValue: () => initial,
      setValue() {},
    };
  } else {
    built = builder(desc, st);
  }

  const layout = built.layout ?? 'stack';
  const errEl = h('p', { class: 'fc-error', id: errId, role: 'alert', hidden: true });
  const descEl = desc?.description ? h('p', { class: 'fc-desc', id: descId }, desc.description) : null;
  let labelEl;
  if (layout === 'fieldset') {
    labelEl = h('legend', { class: 'fc-label', id: labelId }, desc.label);
  } else if (built.labelFor) {
    labelEl = h('label', { class: 'fc-label', id: labelId, for: built.labelFor }, desc.label);
  } else {
    labelEl = h('div', { class: 'fc-label', id: labelId }, desc?.label ?? '');
  }

  let el;
  if (layout === 'inline') {
    const text = h('div', { class: 'fc-inline-text', onclick: () => built.activate?.() }, labelEl, descEl);
    el = h('div', { class: 'fc-field fc-field--boolean' }, text, built.body, errEl);
  } else if (layout === 'fieldset') {
    el = h('fieldset', { class: `fc-field fc-field--${desc.type}` }, labelEl, descEl, built.body, errEl);
  } else {
    el = h('div', { class: `fc-field fc-field--${desc?.type ?? 'unknown'}` }, labelEl, descEl, built.body, errEl);
  }
  el.dataset.field = desc?.field ?? '';
  if (built.group && layout !== 'fieldset') {
    built.group.setAttribute('role', 'group');
    built.group.setAttribute('aria-labelledby', labelId);
  }

  function paintError() {
    const message = serverError ?? localError;
    errEl.textContent = message ?? '';
    errEl.hidden = !message;
    el.classList.toggle('has-error', Boolean(message));
    const ids = [descEl ? descId : null, ...(built.describedBy ?? []), message ? errId : null].filter(Boolean).join(' ');
    for (const target of built.inputs ?? []) {
      if (message) target.setAttribute('aria-invalid', 'true');
      else target.removeAttribute('aria-invalid');
      if (ids) target.setAttribute('aria-describedby', ids);
      else target.removeAttribute('aria-describedby');
    }
  }
  ready = true;
  paintError();

  const api = {
    el,
    getValue: () => built.getValue(),
    // Problems in what the control shows that getValue() hides: [{ path (inside the field, '' = the field), message }].
    validate: () => built.validate?.() ?? [],
    setValue(next) {
      built.setValue(next === undefined ? defaultFor(desc ?? {}) : next);
      serverError = null;
      localError = null;
      built.clearErrors?.();
      paintError();
    },
    setError(message, subPath = '') {
      if (!message) {
        serverError = null;
        built.clearErrors?.();
        paintError();
        return;
      }
      const sub = String(subPath ?? '');
      if (sub && built.routeError?.(sub, message)) return;
      serverError = message;
      paintError();
      built.markInvalid?.(sub);
    },
    setResources(resources) {
      override = resources ?? null;
      built.setResources?.(override);
    },
    focus() {
      if (built.focus) built.focus();
      else built.inputs?.[0]?.focus();
    },
    focusPath(subPath = '') {
      if (subPath && built.focusPath) built.focusPath(String(subPath));
      else api.focus();
    },
    setDisabled(next) {
      const off = Boolean(next);
      el.classList.toggle('is-disabled', off);
      el.toggleAttribute('inert', off);
      if (off) el.setAttribute('aria-disabled', 'true');
      else el.removeAttribute('aria-disabled');
      built.setDisabled?.(off);
    },
    destroy() {
      built.destroy?.();
    },
  };
  if (disabled) api.setDisabled(true);
  return api;
}

function delegateField(desc, opts) {
  const { value, ctx = null, onChange, disabled = false, idPrefix = 'f' } = opts;
  const ctl = createTemplateEditor(desc, { value: value === undefined ? defaultFor(desc) : value, ctx, onChange, idPrefix });
  const api = {
    el: ctl.el,
    getValue: () => ctl.getValue(),
    validate: () => [],
    setValue: (v) => ctl.setValue(v === undefined ? defaultFor(desc) : v),
    setError: (message, subPath = '') => ctl.setError?.(message, subPath),
    setResources: (resources) => ctl.setResources?.(resources),
    focus: () => ctl.focus?.(),
    focusPath: () => ctl.focus?.(),
    setDisabled(next) {
      ctl.el.toggleAttribute('inert', Boolean(next));
      ctl.el.classList.toggle('is-disabled', Boolean(next));
    },
    destroy: () => ctl.destroy?.(),
  };
  if (disabled) api.setDisabled(true);
  return api;
}

/* ------------------------------------------------------------------ */
/* boolean                                                              */
/* ------------------------------------------------------------------ */

function buildBoolean(desc, st) {
  let current = st.value === true ? true : (st.value === null && desc.nullable ? null : false);
  const sw = createSwitch({
    labelId: st.labelId,
    describedBy: desc.description ? st.descId : null,
    onToggle: toggle,
  });
  sw.id = `${st.id}-input`;
  const paint = () => paintSwitch(sw, current === true, false);
  paint();

  function toggle() {
    current = current !== true;
    paint();
    st.changed();
  }

  return {
    layout: 'inline',
    body: sw,
    inputs: [sw],
    getValue: () => current,
    setValue(v) {
      current = v === true ? true : (v === null && desc.nullable ? null : false);
      paint();
    },
    // Clicking the label text flips the switch too.
    activate() {
      sw.focus({ preventScroll: true });
      toggle();
    },
    focus: () => sw.focus(),
  };
}

/* ------------------------------------------------------------------ */
/* int / float                                                          */
/* ------------------------------------------------------------------ */

function buildNumber(desc, st) {
  const isInt = desc.type === 'int';
  const hasRange = Number.isFinite(desc.min) && Number.isFinite(desc.max);
  const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
  let current = num(st.value) ?? (desc.nullable ? null : num(desc.default) ?? (Number.isFinite(desc.min) ? desc.min : 0));

  const input = h('input', {
    type: 'number',
    class: 'fc-input fc-number',
    id: `${st.id}-input`,
    inputmode: isInt ? 'numeric' : 'decimal',
    min: Number.isFinite(desc.min) ? desc.min : null,
    max: Number.isFinite(desc.max) ? desc.max : null,
    step: isInt ? 1 : (desc.step ?? 'any'),
    autocomplete: 'off',
  });
  const unit = desc.unit ? h('span', { class: 'fc-unit' }, desc.unit) : null;
  const rangeHint = hasRange && Math.abs(desc.max) < 1e9
    ? h('p', { class: 'fc-hint', id: `${st.id}-range` }, `Between ${desc.min} and ${desc.max}`)
    : null;

  const paint = () => {
    input.value = current === null ? '' : String(current);
  };
  paint();

  function check(n) {
    if (isInt && !Number.isInteger(n)) return 'Must be a whole number.';
    if (Number.isFinite(desc.min) && n < desc.min) return `Must be at least ${desc.min}.`;
    if (Number.isFinite(desc.max) && n > desc.max) return `Must be at most ${desc.max}.`;
    return null;
  }

  input.addEventListener('input', () => {
    const raw = input.value.trim();
    if (raw === '') {
      if (desc.nullable) current = null;
      st.setLocalError(desc.nullable ? null : 'Enter a number.');
      if (desc.nullable) st.changed();
      return;
    }
    const n = Number(raw);
    if (!Number.isFinite(n)) {
      st.setLocalError('Enter a number.');
      return;
    }
    current = n;
    st.setLocalError(check(n));
    st.changed();
  });
  // On blur an empty or out-of-range entry is settled into a valid number, so what is saved is what is shown.
  input.addEventListener('blur', () => {
    const raw = input.value.trim();
    if (raw === '' && desc.nullable) {
      st.setLocalError(null);
      return;
    }
    let n = raw === '' ? current : Number(raw);
    if (!Number.isFinite(n)) n = current ?? (Number.isFinite(desc.min) ? desc.min : 0);
    if (isInt) n = Math.round(n);
    if (Number.isFinite(desc.min)) n = Math.max(desc.min, n);
    if (Number.isFinite(desc.max)) n = Math.min(desc.max, n);
    const changed = n !== current;
    current = n;
    paint();
    st.setLocalError(null);
    if (changed) st.changed();
  });

  return {
    body: h('div', { class: 'fc-number-wrap' },
      h('div', { class: 'fc-inputgroup' }, input, unit), rangeHint),
    inputs: [input],
    labelFor: input.id,
    describedBy: rangeHint ? [rangeHint.id] : [],
    // What is typed right now, which an emptied or out-of-range box leaves unlike getValue() (the last valid number, until
    // the box loses focus). A save made before that (Ctrl+S) must stop on it instead of sending a number the box does not show.
    validate() {
      const raw = input.value.trim();
      let message = null;
      if (raw === '') message = desc.nullable ? null : 'Enter a number.';
      else message = Number.isFinite(Number(raw)) ? check(Number(raw)) : 'Enter a number.';
      st.setLocalError(message);
      return message ? [{ path: '', message }] : [];
    },
    getValue: () => current,
    setValue(v) {
      current = num(v) ?? (desc.nullable ? null : current);
      paint();
    },
  };
}

/* ------------------------------------------------------------------ */
/* text / url                                                           */
/* ------------------------------------------------------------------ */

const PLACEHOLDER_GROUPS = [
  ['field', 'For this setting'], ['user', 'User'], ['server', 'Server'], ['channel', 'Channel'], ['other', 'Other'],
];

function placeholderGroups(desc, st) {
  const out = new Map(PLACEHOLDER_GROUPS.map(([key, label]) => [key, { label, items: [] }]));
  const seen = new Set();
  const add = (group, token, description) => {
    if (typeof token !== 'string' || !token || seen.has(token)) return;
    seen.add(token);
    (out.get(group) ?? out.get('other')).items.push({ token, description: description ?? '' });
  };
  const extras = Array.isArray(desc.placeholders) ? desc.placeholders : [];
  for (const e of extras) {
    if (typeof e === 'string') add('field', e);
    else if (isObject(e)) add('field', e.token, e.description);
  }
  const builtins = Array.isArray(st.ctx?.placeholders) ? st.ctx.placeholders : [];
  // `placeholderGroups` limits the built-ins, e.g. ['server', 'channel', 'other'] for a message sent without a member.
  const only = Array.isArray(desc.placeholderGroups) ? new Set(desc.placeholderGroups) : null;
  for (const p of builtins) {
    const group = PLACEHOLDER_GROUPS.some(([k]) => k === p.group) ? p.group : 'other';
    if (!only || only.has(group)) add(group, p.token, p.description);
  }
  return [...out.values()].filter((g) => g.items.length);
}

/** How many placeholder chips a text field shows inline; the rest are behind "All placeholders". */
const QUICK_PLACEHOLDERS = 4;

/**
 * The placeholder chips under a text field: an "Insert" row with the field's own tokens first, and the full grouped
 * list behind a disclosure. Every chip inserts its token at the caret.
 */
function placeholderPanel(desc, st, input) {
  const groups = placeholderGroups(desc, st);
  if (!groups.length) return null;
  const panelId = `${st.id}-ph`;
  let selection = null;
  const remember = () => {
    selection = { start: input.selectionStart, end: input.selectionEnd };
  };
  input.addEventListener('blur', remember);

  const chip = (item) => h('button', {
    type: 'button',
    class: 'fc-ph',
    title: item.description || item.token,
    'aria-label': `Insert ${item.token}`,
    onmousedown: (event) => event.preventDefault(),
    onclick: () => insertAtCaret(input, item.token, document.activeElement === input ? null : selection),
  }, item.token);

  const all = groups.flatMap((group) => group.items);
  const panel = h('div', { class: 'fc-ph-panel', id: panelId, hidden: true });
  for (const group of groups) {
    panel.append(h('div', { class: 'fc-ph-group' },
      h('span', { class: 'fc-ph-title' }, group.label),
      h('span', { class: 'fc-ph-chips' }, group.items.map(chip))));
  }
  const toggle = all.length > QUICK_PLACEHOLDERS
    ? h('button', {
      type: 'button',
      class: 'fc-link',
      'aria-expanded': 'false',
      'aria-controls': panelId,
      onclick: () => {
        const open = panel.hidden;
        panel.hidden = !open;
        toggle.setAttribute('aria-expanded', String(open));
      },
    }, `All placeholders (${all.length})`)
    : null;
  const row = h('div', { class: 'fc-insert' },
    h('span', { class: 'fc-insert-label' }, 'Insert'),
    all.slice(0, QUICK_PLACEHOLDERS).map(chip),
    toggle);
  return { row, panel: toggle ? panel : null };
}

/** Sample values the message preview puts in for the common placeholders (others are shown as they are). */
function previewSamples(st) {
  const user = st.ctx?.user;
  const name = (user && (user.globalName || user.username)) || 'nova';
  const server = st.ctx?.guild?.name || 'your server';
  return {
    '{user}': `@${name}`, '{user.mention}': `@${name}`, '{mention}': `@${name}`, '{player}': `@${name}`,
    '{user.name}': name, '{user.username}': name, '{user.tag}': name, '{user.idname}': name, '{player.name}': name,
    '{server}': server, '{server.name}': server, '{server.member_count}': '128', '{server.member_count.ordinal}': '128th',
    '{channel}': '#general', '{channel.mention}': '#general', '{channel.name}': 'general',
    '{level}': '13', '{rank}': '4', '{xp}': '1,240', '{age}': '25', '{achievement}': 'Chatterbox',
  };
}

/** Fills `target` with `text` as the bot would send it: sample values for placeholders and **bold** in bold. */
function paintPreview(target, text, samples) {
  const nodes = [];
  for (const part of String(text).split(/(\*\*[^*\n]+\*\*)/)) {
    const bold = /^\*\*([^*\n]+)\*\*$/.exec(part);
    const pieces = (bold ? bold[1] : part).split(/(\{[\w.]+\})/).filter(Boolean).map((piece) => {
      if (!/^\{[\w.]+\}$/.test(piece)) return document.createTextNode(piece);
      return Object.hasOwn(samples, piece) ? document.createTextNode(samples[piece]) : h('code', { class: 'fc-preview-token' }, piece);
    });
    if (bold) nodes.push(h('strong', {}, pieces));
    else nodes.push(...pieces);
  }
  target.replaceChildren(...nodes);
}

/** The preview under a message field; empty text previews the default message (the field's placeholder). */
function messagePreview(desc, st, input) {
  const text = h('div', { class: 'fc-preview-text' });
  const el = h('div', { class: 'fc-preview' }, h('span', { class: 'fc-preview-label' }, 'Preview'), text);
  const samples = previewSamples(st);
  const paint = () => paintPreview(text, input.value || (typeof desc.placeholder === 'string' ? desc.placeholder : ''), samples);
  input.addEventListener('input', paint);
  paint();
  return { el, paint };
}

function buildText(desc, st) {
  const isUrl = desc.type === 'url';
  const multiline = !isUrl && desc.multiline === true;
  const maxLength = Number.isFinite(desc.maxLength) ? desc.maxLength : (isUrl ? 2048 : null);
  let origNull = st.value === null;
  let touched = false;

  const input = h(multiline ? 'textarea' : 'input', {
    class: `fc-input${multiline ? ' fc-textarea' : ''}`,
    id: `${st.id}-input`,
    type: multiline ? null : (isUrl ? 'url' : 'text'),
    rows: multiline ? (desc.rows ?? 3) : null,
    inputmode: isUrl ? 'url' : null,
    placeholder: typeof desc.placeholder === 'string' ? desc.placeholder : (isUrl ? 'https://' : null),
    autocomplete: 'off',
    spellcheck: isUrl ? 'false' : null,
  });
  const counter = maxLength && !isUrl ? h('span', { class: 'fc-counter', id: `${st.id}-count` }) : null;

  // Without CSS field-sizing the textarea grows by hand.
  const nativeGrow = typeof CSS !== 'undefined' && CSS.supports?.('field-sizing', 'content');
  let observer = null;
  function grow() {
    if (!multiline || nativeGrow || !input.isConnected) return;
    input.style.setProperty('height', 'auto');
    input.style.setProperty('height', `${input.scrollHeight + 2}px`);
  }
  if (multiline && !nativeGrow && typeof ResizeObserver === 'function') {
    observer = new ResizeObserver(() => grow());
    observer.observe(input);
  }

  const textOf = () => (multiline ? input.value.replace(/\r\n?/g, '\n') : input.value);
  const lengthOf = () => (multiline ? textOf().length : textOf().trim().length);

  function paintCounter() {
    if (!counter) return;
    const length = lengthOf();
    counter.textContent = `${length} / ${maxLength}`;
    counter.classList.toggle('is-over', length > maxLength);
    counter.classList.toggle('is-near', length <= maxLength && length >= maxLength * 0.9);
  }

  function shapeMessage() {
    const v = textOf().trim();
    if (!v) return null;
    if (isUrl && !/^https?:\/\/\S+$/i.test(v)) return 'Use a link that starts with http:// or https://.';
    if (desc.pattern) {
      let ok = true;
      try { ok = new RegExp(`^(?:${desc.pattern})$`, 'u').test(v); } catch { /* an unusable pattern is the server's call */ }
      if (!ok) return desc.patternMessage ?? 'This does not have the expected format.';
    }
    return null;
  }

  // Length is checked as you type; the format only after you leave the field (and re-checked while it is flagged).
  let shapeError = null;
  function validateLocal(checkShape) {
    const length = lengthOf();
    if (maxLength && length > maxLength) {
      st.setLocalError(`${length - maxLength} over the ${maxLength} character limit.`);
      return;
    }
    if (checkShape || shapeError) shapeError = shapeMessage();
    st.setLocalError(shapeError);
  }

  input.addEventListener('input', () => {
    touched = true;
    paintCounter();
    validateLocal(false);
    grow();
    st.changed();
  });
  input.addEventListener('blur', () => {
    validateLocal(true);
    grow();
  });
  input.addEventListener('focus', grow);

  const set = (v) => {
    origNull = v === null;
    touched = false;
    shapeError = null;
    input.value = typeof v === 'string' ? v : '';
    paintCounter();
    st.setLocalError(null);
    grow();
  };
  set(st.value);
  requestAnimationFrame(grow);

  // Message-like fields (multi-line, or listing extra tokens) offer the placeholder chips.
  const wantsPlaceholders = Array.isArray(desc.placeholders) || desc.placeholders === true || multiline;
  const ph = !isUrl && desc.placeholders !== false && wantsPlaceholders ? placeholderPanel(desc, st, input) : null;
  const meta = ph || counter ? h('div', { class: 'fc-meta' }, ph?.row, h('span', { class: 'fc-grow' }), counter) : null;
  // A message (multi-line, with placeholders) shows how it will read in Discord.
  const preview = multiline && ph ? messagePreview(desc, st, input) : null;

  return {
    body: h('div', { class: 'fc-text' }, input, meta, ph?.panel, preview?.el),
    inputs: [input],
    labelFor: input.id,
    describedBy: counter ? [counter.id] : [],
    // An untouched null stays null; everything else goes out as typed (the server turns '' into null where allowed).
    getValue: () => (origNull && !touched && input.value === '' ? null : (isUrl ? input.value.trim() : textOf())),
    setValue(v) {
      set(v);
      preview?.paint();
    },
    destroy: () => observer?.disconnect(),
  };
}

/* ------------------------------------------------------------------ */
/* enum / multiEnum                                                     */
/* ------------------------------------------------------------------ */

function buildEnum(desc, st) {
  const options = Array.isArray(desc.options) ? desc.options : [];
  let current = st.value;
  const indexOf = (v) => options.findIndex((o) => o.value === v);
  const useSegments = !desc.nullable && options.length >= 1 && options.length <= 4;
  const descOf = (v) => options[indexOf(v)]?.description ?? '';
  const info = h('p', { class: 'fc-hint', id: `${st.id}-info`, 'aria-live': 'polite' });
  const paintInfo = () => {
    info.textContent = descOf(current);
    info.hidden = !info.textContent;
  };

  if (useSegments) {
    const buttons = options.map((o, i) => h('button', {
      type: 'button',
      class: 'fc-seg-btn',
      role: 'radio',
      'aria-checked': 'false',
      tabindex: '-1',
      onclick: () => choose(i),
      onkeydown: (event) => onKey(event, i),
    }, o.label ?? String(o.value)));
    const group = h('div', { class: 'fc-seg', role: 'radiogroup', 'aria-labelledby': st.labelId }, buttons);

    function paint() {
      const sel = indexOf(current);
      buttons.forEach((b, i) => {
        b.setAttribute('aria-checked', i === sel ? 'true' : 'false');
        b.tabIndex = i === sel || (sel < 0 && i === 0) ? 0 : -1;
      });
      paintInfo();
    }
    function choose(i) {
      current = options[i].value;
      paint();
      st.changed();
    }
    function onKey(event, i) {
      const keys = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 };
      let next = null;
      if (event.key in keys) next = (i + keys[event.key] + options.length) % options.length;
      else if (event.key === 'Home') next = 0;
      else if (event.key === 'End') next = options.length - 1;
      if (next === null) return;
      event.preventDefault();
      choose(next);
      buttons[next].focus();
    }
    paint();
    return {
      body: h('div', { class: 'fc-enum' }, group, info),
      inputs: [],
      describedBy: [info.id],
      getValue: () => current,
      setValue(v) {
        current = v;
        paint();
      },
      focus: () => (buttons.find((b) => b.tabIndex === 0) ?? buttons[0])?.focus(),
    };
  }

  const select = h('select', { class: 'fc-input fc-select', id: `${st.id}-input` });
  const paintSelect = () => {
    select.replaceChildren();
    if (desc.nullable) select.append(h('option', { value: '' }, desc.noneLabel ?? 'None'));
    const known = indexOf(current) >= 0 || (desc.nullable && current === null);
    if (!known) select.append(h('option', { value: '-1', disabled: true }, 'Choose…'));
    options.forEach((o, i) => select.append(h('option', { value: String(i) }, o.label ?? String(o.value))));
    select.value = indexOf(current) >= 0 ? String(indexOf(current)) : (known ? '' : '-1');
    paintInfo();
  };
  paintSelect();
  select.addEventListener('change', () => {
    current = select.value === '' ? null : (options[Number(select.value)]?.value ?? current);
    paintInfo();
    st.changed();
  });
  return {
    body: h('div', { class: 'fc-enum' }, select, info),
    inputs: [select],
    labelFor: select.id,
    describedBy: [info.id],
    getValue: () => current,
    setValue(v) {
      current = v;
      paintSelect();
    },
  };
}

function buildMultiEnum(desc, st) {
  const options = Array.isArray(desc.options) ? desc.options : [];
  const max = Math.min(desc.maxItems ?? options.length, options.length);
  let current = Array.isArray(st.value) ? [...st.value] : [];
  const boxes = options.map((o, i) => {
    const input = h('input', {
      type: 'checkbox', class: 'fc-check-input', id: `${st.id}-c${i}`, onchange: () => toggle(i, input.checked),
    });
    const row = h('label', { class: 'fc-check', for: input.id },
      input,
      h('span', { class: 'fc-check-text' },
        h('span', { class: 'fc-check-label' }, o.label ?? String(o.value)),
        o.description ? h('span', { class: 'fc-check-desc' }, o.description) : null));
    return { input, row };
  });
  const count = h('span', { class: 'fc-counter', id: `${st.id}-count` });
  const allBtn = h('button', { type: 'button', class: 'fc-link', onclick: () => setAll(true) }, 'Select all');
  const noneBtn = h('button', { type: 'button', class: 'fc-link', onclick: () => setAll(false) }, 'Select none');
  const canSelectAll = max >= options.length;

  function paint() {
    const set = new Set(current);
    boxes.forEach((b, i) => {
      const on = set.has(options[i].value);
      b.input.checked = on;
      b.input.disabled = !on && set.size >= max;
      b.row.classList.toggle('is-on', on);
    });
    count.textContent = `${set.size} / ${max}`;
    count.classList.toggle('is-near', set.size >= max);
    allBtn.disabled = set.size >= options.length;
    noneBtn.disabled = set.size === 0;
  }
  function commit(values) {
    current = options.map((o) => o.value).filter((v) => values.has(v));
    paint();
    st.changed();
  }
  function toggle(i, on) {
    const set = new Set(current);
    if (on) {
      if (set.size >= max) {
        paint();
        return;
      }
      set.add(options[i].value);
    } else {
      set.delete(options[i].value);
    }
    commit(set);
  }
  function setAll(on) {
    commit(on ? new Set(options.map((o) => o.value)) : new Set());
  }
  paint();

  const group = h('div', { class: 'fc-checks' },
    h('div', { class: 'fc-meta' }, canSelectAll ? allBtn : null, noneBtn, h('span', { class: 'fc-grow' }), count),
    h('div', { class: 'fc-check-grid' }, boxes.map((b) => b.row)));
  return {
    body: group,
    group,
    inputs: [],
    getValue: () => [...current],
    setValue(v) {
      current = Array.isArray(v) ? [...v] : [];
      paint();
    },
    focus: () => boxes[0]?.input.focus(),
  };
}

/* ------------------------------------------------------------------ */
/* color                                                                */
/* ------------------------------------------------------------------ */

const COLOR_PRESETS = ['#5865F2', '#57F287', '#FEE75C', '#EB459E', '#ED4245', '#F47B67', '#3498DB', '#FFFFFF'];

function parseColor(v) {
  if (Number.isInteger(v) && v >= 0 && v <= 0xffffff) return v;
  if (typeof v === 'string') {
    const t = v.trim();
    const m = HEX_RE.exec(t);
    if (m) return parseInt(m[1], 16);
    const short = HEX3_RE.exec(t);
    if (short) return parseInt([...short[1]].map((c) => c + c).join(''), 16);
  }
  return null;
}

const colorHex = (n) => `#${n.toString(16).padStart(6, '0')}`;

function buildColor(desc, st) {
  const asHex = desc.format === 'hex';
  const minValue = Number.isFinite(desc.min) ? Math.max(desc.min, 0) : null;
  let current = parseColor(st.value);

  const native = h('input', {
    type: 'color', class: 'fc-color-native', id: `${st.id}-picker`, 'aria-label': `${desc.label}: colour picker`,
  });
  const swatch = h('span', { class: 'fc-swatch' }, native);
  const text = h('input', {
    type: 'text', class: 'fc-input fc-hex', id: `${st.id}-input`, maxlength: 7, autocomplete: 'off', spellcheck: 'false',
    placeholder: desc.nullable ? 'Default' : '#5865F2',
  });
  const reset = desc.nullable
    ? h('button', { type: 'button', class: 'fc-text-btn', onclick: () => commit(null, true) }, 'Use default')
    : null;
  const presets = h('div', { class: 'fc-presets', role: 'group', 'aria-label': 'Preset colours' }, COLOR_PRESETS.map((c) => {
    const b = h('button', {
      type: 'button', class: 'fc-preset', title: c, 'aria-label': `Use ${c}`, onclick: () => commit(parseColor(c), true),
    });
    b.style.setProperty('--c', c);
    return b;
  }));

  function paint({ fromText = false } = {}) {
    const shown = current === null ? null : colorHex(current);
    swatch.classList.toggle('is-default', current === null);
    if (shown) {
      swatch.style.setProperty('--c', shown);
      native.value = shown;
    } else {
      swatch.style.removeProperty('--c');
      native.value = '#000000';
    }
    if (!fromText) text.value = shown ? shown.toUpperCase() : '';
    if (reset) reset.disabled = current === null;
  }
  function commit(n, repaintText) {
    if (n !== null && minValue !== null && n < minValue) n = Math.max(minValue, 0x010101);
    current = n;
    paint({ fromText: !repaintText });
    st.setLocalError(null);
    st.changed();
  }
  native.addEventListener('input', () => commit(parseColor(native.value), true));
  text.addEventListener('input', () => {
    const raw = text.value.trim();
    if (raw === '' && desc.nullable) {
      commit(null, false);
      return;
    }
    const n = parseColor(raw);
    if (n === null) {
      st.setLocalError('Use a colour like #5865F2.');
      return;
    }
    commit(n, false);
  });
  text.addEventListener('blur', () => {
    st.setLocalError(null);
    paint();
  });
  paint();

  return {
    body: h('div', { class: 'fc-color' }, h('div', { class: 'fc-color-row' }, swatch, text, reset), presets),
    inputs: [text],
    labelFor: text.id,
    getValue: () => (current === null ? null : (asHex ? colorHex(current).toUpperCase() : current)),
    setValue(v) {
      current = parseColor(v);
      paint();
    },
    focus: () => text.focus(),
  };
}

/* ------------------------------------------------------------------ */
/* channel / channels / role / roles / timezone                         */
/* ------------------------------------------------------------------ */

/**
 * The searchable picker behind channel(s), role(s) and timezone: a combobox that holds one value (a text-like box
 * showing it) or many (removable chips). `cfg` supplies how ids are described and which options are offered.
 */
function buildChooser(desc, st, cfg, multi) {
  const maxItems = multi ? Math.max(1, desc.maxItems ?? BIG_LIST) : 1;
  const nullable = !multi && desc.nullable === true;
  let value = multi ? normalizeIds(st.value) : (typeof st.value === 'string' && st.value ? st.value : null);
  let combo = null;

  const input = h('input', {
    type: 'text',
    class: 'fc-chooser-input',
    id: `${st.id}-input`,
    role: 'combobox',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    'aria-autocomplete': 'list',
    'aria-expanded': 'false',
    'aria-haspopup': 'listbox',
    'aria-controls': `${st.id}-list`,
  });
  const lead = multi ? null : h('span', { class: 'fc-lead', 'aria-hidden': 'true' });
  const chipsEl = multi ? h('span', { class: 'fc-chips' }) : null;
  const clearBtn = nullable
    ? h('button', {
      type: 'button',
      class: 'fc-clear',
      'aria-label': `Clear ${desc.label}`,
      onclick: () => {
        value = null;
        paint();
        st.changed();
        input.focus();
      },
    }, icon('x'))
    : null;
  const caret = h('span', { class: 'fc-caret', 'aria-hidden': 'true' }, icon('chevron'));
  const box = h('div', { class: `fc-box fc-chooser${multi ? ' is-multi' : ''}` }, lead, chipsEl, input, clearBtn, caret);
  const root = h('div', { class: 'fc-picker' }, box);
  const note = h('div', { class: 'fc-meta fc-note-row', id: `${st.id}-note` });

  box.addEventListener('click', (event) => {
    if (event.target.closest('button')) return;
    input.focus();
    if (combo.isOpen && event.target !== input) combo.close();
    else combo.open();
  });

  const optionLabel = (d) => d.label;
  function renderOption(d) {
    const selected = multi ? value.includes(d.value) : d.value === value;
    return [
      h('span', { class: 'fc-opt-main' },
        decoNode(d),
        h('span', { class: 'fc-opt-label' }, optionLabel(d)),
        d.kind && d.glyph !== '#' ? h('span', { class: 'sr-only' }, ` (${d.kind})`) : null),
      d.hint || cfg.optionHint?.(d.value) ? h('span', { class: 'fc-opt-hint' }, d.hint ?? cfg.optionHint(d.value)) : null,
      selected ? h('span', { class: 'fc-opt-check' }, icon('check')) : null,
    ];
  }

  function getGroups(query) {
    const result = cfg.groups(query);
    const full = multi && value.length >= maxItems;
    let groups = result.groups.map((g) => ({
      ...g,
      options: g.options.map((o) => (full && !value.includes(o.value)
        ? { ...o, disabled: true, hint: 'Limit reached' } : o)),
    }));
    if (nullable && !query.trim()) {
      groups = [{ options: [{ value: null, label: desc.noneLabel ?? 'None', none: true }] }, ...groups];
    }
    return { groups, note: result.note };
  }

  function chip(id) {
    const d = cfg.describe(id);
    const name = d.missing ? cfg.deleted : d.label;
    return h('span', { class: `fc-chip${d.missing ? ' is-missing' : ''}`, title: d.missing ? `${cfg.deleted} (${id})` : d.label },
      d.missing ? icon('alert') : decoNode(d),
      h('span', { class: 'fc-chip-label' }, name),
      d.missing || d.unknown ? h('span', { class: 'fc-chip-id' }, d.missing ? `(${id})` : '') : null,
      h('button', {
        type: 'button',
        class: 'fc-chip-x',
        'aria-label': `Remove ${name}${d.missing ? ` ${id}` : ''}`,
        onclick: () => {
          value = value.filter((v) => v !== id);
          paint();
          combo.refresh();
          st.changed();
          input.focus();
        },
      }, icon('x')));
  }

  function paintNote() {
    note.replaceChildren();
    const res = st.getRes();
    const blocked = cfg.needsResources && !res;
    if (blocked) {
      const text = `The ${cfg.noun} list is unavailable, so IDs are shown. You can still paste an ID.`;
      note.append(h('span', { class: 'fc-hint' }, text));
    }
    if (blocked && typeof st.ctx?.reloadResources === 'function') note.append(reloadButton());
    note.append(h('span', { class: 'fc-grow' }));
    if (multi && desc.maxItems) {
      const full = value.length >= maxItems;
      note.append(h('span', { class: `fc-counter${full ? ' is-near' : ''}` }, `${value.length} / ${maxItems}`));
    }
    note.hidden = note.childNodes.length <= 1;
  }

  function reloadButton() {
    const btn = h('button', { type: 'button', class: 'fc-link' }, icon('refresh'), 'Refresh');
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      try {
        await st.reload();
      } catch { /* the page reports load errors itself */ }
      btn.disabled = false;
    });
    return btn;
  }

  function paint() {
    if (multi) {
      chipsEl.replaceChildren(...value.map(chip));
      input.placeholder = value.length >= maxItems ? 'Limit reached' : (value.length ? '' : cfg.placeholder);
    } else {
      const d = value ? cfg.describe(value) : null;
      if (!(combo?.isOpen && combo.query)) input.value = d ? (d.missing ? `${cfg.deleted} (${value})` : d.label) : '';
      input.placeholder = nullable ? (desc.noneLabel ?? 'None') : cfg.placeholder;
      lead.replaceChildren(...(d && !d.missing && decoNode(d) ? [decoNode(d)] : []));
      box.classList.toggle('is-missing', Boolean(d?.missing));
      box.classList.toggle('has-lead', lead.childNodes.length > 0);
      box.classList.toggle('has-clear', Boolean(clearBtn && value));
      if (clearBtn) clearBtn.hidden = !value;
    }
    paintNote();
  }

  function onPick(d) {
    if (multi) {
      if (value.includes(d.value)) value = value.filter((v) => v !== d.value);
      else if (value.length < maxItems) value = [...value, d.value];
      input.value = '';
      paint();
      combo.setQuery('');
    } else {
      value = d.value ?? null;
      combo.close();
      paint();
    }
    st.changed();
  }

  combo = createCombobox({
    id: st.id,
    input,
    anchor: box,
    root,
    label: `${desc.label}: ${cfg.noun} list`,
    multi,
    getGroups,
    renderOption,
    isSelected: (d) => (multi ? value.includes(d.value) : d.value === value),
    onPick,
    onOpenChange(open) {
      if (open) {
        if (!multi) input.select();
        paint();
      } else {
        if (multi) input.value = '';
        paint();
      }
    },
    footer: cfg.needsResources && typeof st.ctx?.reloadResources === 'function' ? reloadButton() : null,
    emptyText: `No ${cfg.noun}s match.`,
  });
  paint();

  return {
    body: h('div', { class: 'fc-chooser-wrap' }, root, note),
    inputs: [input],
    labelFor: input.id,
    describedBy: [note.id],
    getValue: () => (multi ? [...value] : value),
    setValue(v) {
      value = multi ? normalizeIds(v) : (typeof v === 'string' && v ? v : null);
      paint();
    },
    setResources() {
      paint();
      combo.refresh();
    },
    setDisabled: (d) => combo.setDisabled(d),
    focus: () => input.focus(),
    destroy: () => combo.destroy(),
  };
}

/* ------------------------------------------------------------------ */
/* user (member search)                                                 */
/* ------------------------------------------------------------------ */

const SEARCH_DEBOUNCE_MS = 250;
const SEARCH_RETRY_MIN_MS = 1000;
const SEARCH_RETRY_MAX_MS = 10000;
const MEMBER_CACHE_MAX = 300;

// Members seen by any picker, so a chosen user's name is not fetched twice. Bounded: the oldest entry goes first.
const memberCache = new Map();
const memberLookups = new Map(); // in-flight `${guildId}:${userId}` lookups, shared between pickers

const MENTION_RE = /^<@!?(\d{17,20})>$/;

/** The user id in a pasted id or mention (`<@123…>`), else null. */
function asUserId(text) {
  const t = String(text ?? '').trim();
  const mention = MENTION_RE.exec(t);
  return mention ? mention[1] : (isSnowflake(t) ? t : null);
}

// The page's CSP only lets Discord's CDN supply images; anything else would be blocked, so it is not requested.
function cdnImage(url) {
  try {
    return new URL(url).hostname === 'cdn.discordapp.com' ? url : null;
  } catch {
    return null;
  }
}

/** A member from GET /api/guilds/:id/members, or null when the entry is malformed. */
function toMember(raw) {
  if (!isObject(raw) || !isSnowflake(raw.id)) return null;
  const text = (v) => (typeof v === 'string' && v.trim() ? v.trim() : '');
  const username = text(raw.username);
  return {
    id: raw.id,
    username,
    name: text(raw.nick) || text(raw.globalName) || username || raw.id,
    avatarUrl: typeof raw.avatarUrl === 'string' ? cdnImage(raw.avatarUrl) : null,
    bot: raw.bot === true,
  };
}

function rememberMember(guildId, member) {
  const key = `${guildId}:${member.id}`;
  memberCache.delete(key);
  memberCache.set(key, member);
  if (memberCache.size > MEMBER_CACHE_MAX) memberCache.delete(memberCache.keys().next().value);
}

const cachedMember = (guildId, id) => memberCache.get(`${guildId}:${id}`) ?? null;

/** Searches the server's members by name; a pasted id finds that one member. Rejects with an ApiError. */
async function searchMembers(guildId, query) {
  const data = await request(`/api/guilds/${encodeURIComponent(guildId)}/members?query=${encodeURIComponent(query)}`);
  const members = (Array.isArray(data?.members) ? data.members : []).map(toMember).filter(Boolean);
  for (const member of members) rememberMember(guildId, member);
  return members;
}

/** One member by id: cached, else looked up once (concurrent callers share the request). Null = not in the server. */
function findMember(guildId, id) {
  const hit = cachedMember(guildId, id);
  if (hit) return Promise.resolve(hit);
  const key = `${guildId}:${id}`;
  let pending = memberLookups.get(key);
  if (!pending) {
    pending = searchMembers(guildId, id)
      .then((members) => members.find((m) => m.id === id) ?? null)
      .finally(() => memberLookups.delete(key));
    memberLookups.set(key, pending);
  }
  return pending;
}

/** The member's picture, name and (when it differs) @username, as one block. */
function memberLine(member, { handle = true } = {}) {
  const pic = avatar(member.name, member.avatarUrl, { size: 'xs', round: true });
  pic.classList.add('fc-user-av');
  const showHandle = handle && member.username && member.username !== member.name;
  return [
    pic,
    h('span', { class: 'fc-user-text' },
      h('span', { class: 'fc-user-name' }, member.name),
      showHandle ? h('span', { class: 'fc-user-handle' }, `@${member.username}`) : null),
    member.bot ? h('span', { class: 'fc-bot-tag' }, 'Bot') : null,
  ];
}

/**
 * A server member: a searchable combobox that asks the dashboard's member search while the admin types (debounced),
 * accepts a pasted user id, and shows the chosen member as a chip. The value is the id (or null).
 */
function buildUser(desc, st) {
  const nullable = desc.nullable === true;
  const noneLabel = desc.noneLabel ?? 'None';
  const guildId = () => (typeof st.ctx?.guildId === 'string' && st.ctx.guildId ? st.ctx.guildId : null);
  let value = isSnowflake(st.value) ? st.value : null;
  let member = null; // the resolved member of `value`
  let lookup = 'idle'; // idle | loading | missing (not in the server) | failed
  let lookupSeq = 0;
  let results = []; // members found for `resultsQuery`
  let resultsQuery = null;
  let searchState = 'idle'; // idle | loading | done | limited | failed
  let searchError = '';
  let searchSeq = 0;
  let searchTimer = 0;
  let retryTimer = 0;
  let retriedFor = null;
  let destroyed = false;
  let combo = null;

  const input = h('input', {
    type: 'text',
    class: 'fc-chooser-input fc-user-input',
    id: `${st.id}-input`,
    role: 'combobox',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    'aria-autocomplete': 'list',
    'aria-expanded': 'false',
    'aria-haspopup': 'listbox',
    'aria-controls': `${st.id}-list`,
  });
  const chipEl = h('span', { class: 'fc-chips' });
  const caret = h('span', { class: 'fc-caret', 'aria-hidden': 'true' }, icon('chevron'));
  const box = h('div', { class: 'fc-box fc-chooser fc-user is-multi' }, chipEl, input, caret);
  const root = h('div', { class: 'fc-picker' }, box);
  const live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });

  box.addEventListener('click', (event) => {
    if (event.target.closest('button')) return;
    input.focus();
    if (combo.isOpen && event.target !== input) combo.close();
    else combo.open();
  });

  /* ---- the chosen member ---- */

  function chip() {
    const unknown = lookup === 'missing';
    let body;
    if (member) {
      body = memberLine(member);
    } else {
      const pic = avatar(unknown ? '?' : value, null, { size: 'xs', round: true });
      pic.classList.add('fc-user-av');
      let label = `User ${value}`;
      if (unknown) label = 'Unknown user';
      else if (lookup === 'loading') label = 'Loading…';
      body = [
        unknown ? icon('alert') : pic,
        h('span', { class: 'fc-user-text' },
          h('span', { class: 'fc-user-name' }, label),
          unknown ? h('span', { class: 'fc-user-handle is-id' }, `(${value})`) : null),
      ];
    }
    const name = member ? member.name : (unknown ? `unknown user ${value}` : `user ${value}`);
    const title = unknown ? `Unknown user (${value})` : (member ? `${member.name} (${member.id})` : `User ${value}`);
    return h('span', { class: `fc-chip fc-member${unknown ? ' is-missing' : ''}`, title },
      body,
      nullable
        ? h('button', {
          type: 'button',
          class: 'fc-chip-x',
          'aria-label': `Remove ${name}`,
          onclick: () => {
            value = null;
            member = null;
            lookup = 'idle';
            lookupSeq += 1;
            paint();
            st.changed();
            input.focus();
          },
        }, icon('x'))
        : null);
  }

  function paint() {
    chipEl.replaceChildren(...(value ? [chip()] : []));
    if (value) input.placeholder = 'Search to change…';
    else input.placeholder = nullable ? `${noneLabel} — search or paste an ID` : 'Search members or paste an ID';
    box.classList.toggle('is-missing', lookup === 'missing');
  }

  async function resolve(id) {
    const gid = guildId();
    const hit = gid ? cachedMember(gid, id) : null;
    if (hit) {
      member = hit;
      lookup = 'idle';
      paint();
      return;
    }
    member = null;
    if (!gid) {
      lookup = 'failed';
      paint();
      return;
    }
    const seq = (lookupSeq += 1);
    lookup = 'loading';
    paint();
    try {
      const found = await findMember(gid, id);
      if (destroyed || seq !== lookupSeq || value !== id) return;
      member = found;
      lookup = found ? 'idle' : 'missing';
    } catch {
      if (destroyed || seq !== lookupSeq || value !== id) return;
      lookup = 'failed';
    }
    paint();
  }

  function choose(id, known = null) {
    value = id;
    member = known;
    lookup = 'idle';
    lookupSeq += 1;
    paint();
    if (id && !known) resolve(id);
    live.textContent = known ? `Selected ${known.name}` : (id ? 'Selected user ID' : `${noneLabel} selected`);
    st.changed();
  }

  /* ---- searching ---- */

  function stopSearch() {
    searchSeq += 1;
    clearTimeout(searchTimer);
    clearTimeout(retryTimer);
    searchTimer = 0;
    retryTimer = 0;
  }

  function onQuery(raw) {
    const q = raw.trim();
    stopSearch();
    results = [];
    resultsQuery = null;
    const gid = guildId();
    const pasted = asUserId(q);
    if (!q || !gid) {
      searchState = 'idle';
    } else if (pasted && cachedMember(gid, pasted)) {
      results = [cachedMember(gid, pasted)];
      resultsQuery = q;
      searchState = 'done';
    } else {
      searchState = 'loading';
      searchTimer = setTimeout(() => runSearch(q), SEARCH_DEBOUNCE_MS);
    }
    combo.refresh();
  }

  async function runSearch(q) {
    const gid = guildId();
    if (!gid || destroyed) return;
    const seq = (searchSeq += 1);
    try {
      // A mention or id searches by id; a leading "@" (as in @username) is not part of any name.
      const found = await searchMembers(gid, asUserId(q) ?? (q.replace(/^@+/, '').trim() || q));
      if (destroyed || seq !== searchSeq) return;
      results = found;
      resultsQuery = q;
      searchState = 'done';
    } catch (err) {
      if (destroyed || seq !== searchSeq) return;
      results = [];
      resultsQuery = q;
      if (err?.status === 429) {
        searchState = 'limited';
        // One automatic retry once the server's wait is over, while the admin is still looking at this query.
        if (retriedFor !== q) {
          retriedFor = q;
          const wait = Math.min(SEARCH_RETRY_MAX_MS, Math.max(SEARCH_RETRY_MIN_MS, (Number(err.retryAfter) || 2) * 1000));
          retryTimer = setTimeout(() => {
            if (destroyed || !combo.isOpen || input.value.trim() !== q) return;
            searchState = 'loading';
            combo.refresh();
            runSearch(q);
          }, wait);
        }
      } else {
        searchState = 'failed';
        searchError = err?.status === 401 ? 'Your session has expired. Reload the page.' : 'Could not search members. Try again.';
      }
    }
    combo.refresh();
  }

  function getGroups(query) {
    const q = query.trim();
    const groups = [];
    if (nullable && !q) groups.push({ options: [{ value: null, label: noneLabel, none: true }] });
    const pasted = asUserId(q);
    if (!guildId()) {
      const note = 'Member search is not available here. Paste a user ID.';
      if (pasted) return { groups: [{ options: [{ value: pasted, label: `Use user ID ${pasted}`, idOnly: true }] }] };
      return { groups, note };
    }
    if (!q) return { groups, note: 'Type a name or username, or paste a user ID.', status: 'Type to search members' };

    const options = resultsQuery === q ? results.map((m) => ({ value: m.id, member: m })) : [];
    if (pasted && !options.some((o) => o.value === pasted)) {
      options.unshift({ value: pasted, label: `Use user ID ${pasted}`, idOnly: true });
    }
    if (options.length) groups.push({ options });

    let note = null;
    let status = null;
    switch (searchState) {
      case 'loading':
        note = 'Searching…';
        status = 'Searching…';
        break;
      case 'limited':
        note = 'Slow down: too many searches at once. Trying again in a moment.';
        status = 'Too many searches. Slow down.';
        break;
      case 'failed':
        note = searchError;
        status = searchError;
        break;
      case 'done':
        if (pasted && !results.some((m) => m.id === pasted)) note = 'Not found in this server. You can still use the ID.';
        else if (!results.length) note = `No members match "${q.length > 40 ? `${q.slice(0, 40)}…` : q}".`;
        status = results.length ? `${results.length} ${plural(results.length, 'member')} found` : 'No members found';
        break;
      default:
        break;
    }
    return { groups, note, status };
  }

  function renderOption(d) {
    const selected = d.value === value;
    let main;
    if (d.none) main = h('span', { class: 'fc-opt-label' }, d.label);
    else if (d.member) main = memberLine(d.member);
    else main = [h('span', { class: 'fc-opt-label' }, d.label)];
    return [
      h('span', { class: `fc-opt-main${d.member ? ' fc-user-opt' : ''}` }, main),
      selected ? h('span', { class: 'fc-opt-check' }, icon('check')) : null,
    ];
  }

  combo = createCombobox({
    id: st.id,
    input,
    anchor: box,
    root,
    label: `${desc.label}: member list`,
    getGroups,
    renderOption,
    isSelected: (d) => d.value === value,
    onPick(d) {
      input.value = '';
      combo.close();
      choose(d.value ?? null, d.member ?? null);
    },
    onOpenChange(open) {
      if (open) {
        retriedFor = null;
        return;
      }
      stopSearch();
      searchState = 'idle';
      results = [];
      resultsQuery = null;
      // An id typed or pasted and left behind counts as chosen.
      const typed = asUserId(input.value);
      input.value = '';
      if (typed && typed !== value) choose(typed, guildId() ? cachedMember(guildId(), typed) : null);
      else paint();
    },
    emptyText: 'No members match.',
  });
  input.addEventListener('input', () => onQuery(input.value));
  // A pasted id is taken straight away; the name is looked up afterwards.
  input.addEventListener('paste', (event) => {
    const id = asUserId(event.clipboardData?.getData('text'));
    const replaces = !input.value || (input.selectionStart === 0 && input.selectionEnd === input.value.length);
    if (!id || !replaces) return;
    event.preventDefault();
    input.value = '';
    combo.close();
    choose(id, guildId() ? cachedMember(guildId(), id) : null);
  });
  root.append(live);
  paint();
  if (value) resolve(value);

  return {
    body: h('div', { class: 'fc-chooser-wrap' }, root),
    inputs: [input],
    labelFor: input.id,
    getValue: () => value,
    setValue(v) {
      const next = isSnowflake(v) ? v : null;
      if (next === value && (member || !next)) return;
      value = next;
      member = null;
      lookup = 'idle';
      lookupSeq += 1;
      paint();
      if (next) resolve(next);
    },
    setDisabled(off) {
      combo.setDisabled(off);
      if (off) stopSearch();
    },
    focus: () => input.focus(),
    destroy() {
      destroyed = true;
      stopSearch();
      combo.destroy();
    },
  };
}

/* ------------------------------------------------------------------ */
/* stringList                                                           */
/* ------------------------------------------------------------------ */

function buildStringList(desc, st) {
  const maxItems = desc.maxItems ?? BIG_LIST;
  const itemMax = desc.itemMaxLength ?? 200;
  const itemMin = desc.itemMinLength ?? 1;
  const commaSplits = desc.commaSeparated !== false;
  const hintText = typeof desc.placeholder === 'string' ? desc.placeholder : 'Type, then press Enter';
  let items = [];
  let invalidIndex = -1;

  const input = h('input', {
    type: 'text',
    class: 'fc-taginput',
    id: `${st.id}-input`,
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    placeholder: hintText,
  });
  const chipsEl = h('span', { class: 'fc-chips' });
  const box = h('div', {
    class: 'fc-box fc-tags',
    onclick: (event) => {
      if (!event.target.closest('button')) input.focus();
    },
  }, chipsEl, input);
  const hint = h('span', { class: 'fc-hint', id: `${st.id}-hint`, 'aria-live': 'polite' });
  const counter = h('span', { class: 'fc-counter', id: `${st.id}-count` });
  const clearAll = h('button', { type: 'button', class: 'fc-link', onclick: () => {
    items = [];
    paint();
    st.changed();
    input.focus();
  } }, 'Clear all');
  const meta = h('div', { class: 'fc-meta' }, hint, h('span', { class: 'fc-grow' }), clearAll, counter);

  function tagChip(item, i) {
    return h('span', { class: `fc-chip fc-tag${i === invalidIndex ? ' is-invalid' : ''}`, title: item },
      h('span', { class: 'fc-chip-label' }, item),
      h('button', {
        type: 'button',
        class: 'fc-chip-x',
        'aria-label': `Remove ${item}`,
        onclick: () => {
          items = items.filter((_, k) => k !== i);
          invalidIndex = -1;
          hint.textContent = '';
          paint();
          st.changed();
          input.focus();
        },
      }, icon('x')));
  }

  function paint() {
    chipsEl.replaceChildren(...items.map(tagChip));
    counter.textContent = `${items.length} / ${maxItems}`;
    counter.classList.toggle('is-near', items.length >= maxItems);
    clearAll.hidden = items.length === 0;
    input.placeholder = items.length >= maxItems ? 'List is full' : hintText;
  }

  /** Adds one entry; returns null on success or the reason it was refused. */
  function tryAdd(raw) {
    let s = raw.trim();
    if (!s) return 'empty';
    if (desc.lowercase) s = s.toLowerCase();
    if (s.length > itemMax) return `"${s.slice(0, 24)}…" is too long (at most ${itemMax} characters).`;
    if (s.length < itemMin) return `Entries need at least ${itemMin} characters.`;
    if (desc.itemPattern) {
      let ok = true;
      try { ok = new RegExp(`^(?:${desc.itemPattern})$`, 'u').test(s); } catch { /* the server decides */ }
      if (!ok) return `"${s.slice(0, 24)}" ${desc.itemPatternMessage ?? 'has an invalid format.'}`;
    }
    if (items.some((x) => x.toLowerCase() === s.toLowerCase())) return `"${s.slice(0, 24)}" is already in the list.`;
    if (items.length >= maxItems) return `The list is full (at most ${maxItems}).`;
    items.push(s);
    return null;
  }

  function addMany(parts) {
    let added = 0;
    let firstProblem = '';
    let skipped = 0;
    for (const part of parts) {
      const problem = tryAdd(part);
      if (problem === null) added += 1;
      else if (problem !== 'empty') {
        skipped += 1;
        firstProblem ||= problem;
      }
    }
    invalidIndex = -1;
    if (added) {
      paint();
      st.changed();
    }
    if (parts.length > 1) {
      hint.textContent = skipped ? `Added ${added}, skipped ${skipped}. ${firstProblem}` : `Added ${added}.`;
    } else {
      hint.textContent = firstProblem;
    }
    return { added, skipped };
  }

  input.addEventListener('keydown', (event) => {
    if (event.isComposing) return;
    if (event.key === 'Enter' || (event.key === ',' && commaSplits)) {
      event.preventDefault();
      const { skipped } = addMany([input.value]);
      if (!skipped) input.value = '';
    } else if (event.key === 'Backspace' && input.value === '' && items.length) {
      const removed = items.pop();
      invalidIndex = -1;
      hint.textContent = `Removed ${removed}.`;
      paint();
      st.changed();
    }
  });
  input.addEventListener('input', () => {
    hint.textContent = '';
    if (commaSplits && input.value.includes(',')) {
      const parts = input.value.split(',');
      const rest = parts.pop();
      addMany(parts);
      input.value = rest;
    }
  });
  input.addEventListener('paste', (event) => {
    const text = event.clipboardData?.getData('text') ?? '';
    if (!/\r?\n/.test(text)) return;
    event.preventDefault();
    const lines = text.split(/\r?\n/).flatMap((line) => (commaSplits ? line.split(',') : [line]));
    addMany(lines);
  });
  input.addEventListener('blur', () => {
    if (input.value.trim() && !addMany([input.value]).skipped) input.value = '';
  });

  function set(v) {
    items = Array.isArray(v) ? v.filter((x) => typeof x === 'string') : [];
    invalidIndex = -1;
    hint.textContent = '';
    paint();
  }
  set(st.value);

  return {
    body: h('div', { class: 'fc-taglist' }, box, meta),
    inputs: [input],
    labelFor: input.id,
    describedBy: [hint.id, counter.id],
    getValue: () => [...items],
    setValue: set,
    markInvalid(sub) {
      const i = Number.parseInt(sub, 10);
      if (Number.isInteger(i) && i >= 0 && i < items.length) {
        invalidIndex = i;
        paint();
      }
    },
    clearErrors() {
      if (invalidIndex !== -1) {
        invalidIndex = -1;
        paint();
      }
    },
    focus: () => input.focus(),
  };
}

/* ------------------------------------------------------------------ */
/* datetime / time                                                      */
/* ------------------------------------------------------------------ */

function isoToLocalInput(iso) {
  const d = new Date(iso);
  if (typeof iso !== 'string' || Number.isNaN(d.getTime())) return '';
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}T${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

function localInputToIso(local) {
  if (!local) return null;
  const d = new Date(local);
  return Number.isNaN(d.getTime()) ? null : d.toISOString();
}

function browserZoneNote() {
  let zone = '';
  try { zone = Intl.DateTimeFormat().resolvedOptions().timeZone ?? ''; } catch { /* no Intl */ }
  const offset = -new Date().getTimezoneOffset();
  const sign = offset >= 0 ? '+' : '-';
  const label = `UTC${sign}${pad2(Math.floor(Math.abs(offset) / 60))}:${pad2(Math.abs(offset) % 60)}`;
  return `Shown in your browser's time zone${zone ? ` (${zone}, ${label})` : ` (${label})`}.`;
}

function buildDatetime(desc, st) {
  let iso = typeof st.value === 'string' && st.value ? st.value : null;
  const input = h('input', { type: 'datetime-local', class: 'fc-input fc-datetime', id: `${st.id}-input`, step: 60 });
  const clear = desc.nullable
    ? h('button', { type: 'button', class: 'btn btn-secondary btn-sm', onclick: () => {
      iso = null;
      paint();
      st.changed();
    } }, 'Clear')
    : null;
  const note = h('p', { class: 'fc-hint', id: `${st.id}-note` });
  const utc = h('span', {});

  function paint() {
    input.value = iso ? isoToLocalInput(iso) : '';
    const parsed = iso ? new Date(iso) : null;
    utc.textContent = parsed && !Number.isNaN(parsed.getTime())
      ? ` In UTC: ${parsed.toISOString().slice(0, 16).replace('T', ' ')}.` : '';
    if (clear) clear.disabled = !iso;
  }
  paint();
  note.append(browserZoneNote(), utc);

  input.addEventListener('input', () => {
    // An untouched minute keeps the stored seconds, so reading a value back never reports a change.
    iso = input.value === isoToLocalInput(iso) ? iso : localInputToIso(input.value);
    const parsed = iso ? new Date(iso) : null;
    utc.textContent = parsed ? ` In UTC: ${parsed.toISOString().slice(0, 16).replace('T', ' ')}.` : '';
    if (clear) clear.disabled = !iso;
    st.changed();
  });

  return {
    body: h('div', { class: 'fc-datetime-wrap' }, h('div', { class: 'fc-inputgroup' }, input, clear), note),
    inputs: [input],
    labelFor: input.id,
    describedBy: [note.id],
    getValue: () => iso,
    setValue(v) {
      iso = typeof v === 'string' && v ? v : null;
      paint();
    },
  };
}

function buildTime(desc, st) {
  const input = h('input', { type: 'time', class: 'fc-input fc-time', id: `${st.id}-input`, step: 60 });
  const clean = (v) => (typeof v === 'string' && /^([01]\d|2[0-3]):[0-5]\d$/.test(v) ? v : '');
  input.value = clean(st.value);
  input.addEventListener('input', () => st.changed());
  return {
    body: input,
    inputs: [input],
    labelFor: input.id,
    getValue: () => (input.value === '' && desc.nullable ? null : input.value),
    setValue(v) {
      input.value = clean(v);
    },
  };
}

/* ------------------------------------------------------------------ */
/* emoji                                                                */
/* ------------------------------------------------------------------ */

const COMMON_EMOJI = [
  '😀', '😂', '😍', '🥳', '😎', '🤔', '😢', '😡', '👍', '👎', '👏', '🙏', '🔥', '✨', '🎉', '🎂',
  '🎁', '❤️', '💜', '💙', '⭐', '🌟', '✅', '❌', '⚠️', '📢', '🔔', '🎮', '🎵', '🏆', '💎', '🪙',
];

function emojiImageUrl(id, animated) {
  return `${EMOJI_CDN}/${id}.webp?size=48${animated ? '&animated=true' : ''}`;
}

function emojiImage(id, name, animated, size) {
  const img = h('img', {
    class: `fc-emoji-img fc-emoji-${size}`, src: emojiImageUrl(id, animated), alt: `:${name}:`,
    loading: size === 'lg' ? 'lazy' : null, decoding: 'async', referrerpolicy: 'no-referrer',
  });
  img.addEventListener('error', () => img.replaceWith(h('span', { class: 'fc-emoji-fallback' }, `:${name}:`)), { once: true });
  return img;
}

function buildEmoji(desc, st) {
  let text = typeof st.value === 'string' ? st.value : '';
  let origNull = st.value === null;
  let touched = false;
  const input = h('input', {
    type: 'text', class: 'fc-input fc-emoji-input', id: `${st.id}-input`, autocomplete: 'off', spellcheck: 'false',
    placeholder: '🙂 or <:name:123…>',
  });
  const preview = h('span', { class: 'fc-emoji-preview', 'aria-hidden': 'true' });
  const pickBtn = h('button', {
    type: 'button',
    class: 'btn btn-secondary btn-sm',
    'aria-haspopup': 'dialog',
    'aria-expanded': 'false',
    onclick: () => togglePicker(),
  }, localIcon('smile'), 'Pick');
  const clearBtn = desc.nullable
    ? h('button', { type: 'button', class: 'btn btn-ghost btn-sm', onclick: () => {
      setText('');
      st.changed();
      input.focus();
    } }, 'Clear')
    : null;
  const row = h('div', { class: 'fc-emoji-row' }, preview, input, pickBtn, clearBtn);
  const root = h('div', { class: 'fc-emoji' }, row);

  function paintPreview() {
    const v = text.trim();
    const m = CUSTOM_EMOJI_RE.exec(v);
    preview.replaceChildren();
    if (m) preview.append(emojiImage(m[3], m[2], m[1] === 'a', 'md'));
    else if (v) preview.append(document.createTextNode(Array.from(v).slice(0, 4).join('')));
    else preview.append(localIcon('smile'));
    preview.classList.toggle('is-empty', !v);
    if (clearBtn) clearBtn.disabled = !v;
    const bad = v && !CUSTOM_EMOJI_RE.test(v) && !EMOJI_HINT_RE.test(v);
    st.setLocalError(bad ? 'Enter one emoji, or pick one.' : null);
  }
  function setText(v) {
    text = v;
    input.value = v;
    paintPreview();
  }
  input.addEventListener('input', () => {
    text = input.value;
    touched = true;
    paintPreview();
    st.changed();
  });

  // Picker popover.
  const search = h('input', {
    type: 'search', class: 'fc-input fc-emoji-search', placeholder: 'Search server emojis', 'aria-label': 'Search server emojis',
    autocomplete: 'off',
  });
  const grid = h('div', { class: 'fc-emoji-grid-wrap' });
  const panel = h('div', { class: 'fc-emoji-panel', role: 'dialog', 'aria-label': 'Choose an emoji' }, search, grid);
  const pop = createPopover(row, {
    root, className: 'fc-pop-emoji', minWidth: 300, maxHeight: 340, onDismiss: () => closePicker(false),
  });
  pop.el.append(panel);
  root.append(pop.el);

  function chooseEmoji(value) {
    closePicker(true);
    setText(value);
    touched = true;
    st.changed();
  }
  function emojiGrid(title, buttons) {
    return [h('div', { class: 'fc-group-label' }, title), h('div', { class: 'fc-emoji-grid' }, buttons)];
  }
  function renderGrid() {
    const q = search.value.trim().toLowerCase();
    const res = st.getRes();
    const matches = (e) => isSnowflake(e.id) && (!q || e.name.toLowerCase().includes(q));
    const custom = Array.isArray(res?.emojis) ? res.emojis.filter(matches) : [];
    grid.replaceChildren();
    if (custom.length) {
      grid.append(...emojiGrid('Server emojis', custom.map((e) => h('button', {
        type: 'button', class: 'fc-emoji-btn', title: `:${e.name}:`, 'aria-label': `:${e.name}:`,
        onclick: () => chooseEmoji(`<${e.animated ? 'a' : ''}:${e.name}:${e.id}>`),
      }, emojiImage(e.id, e.name, e.animated, 'lg')))));
    } else if (res && !q) {
      grid.append(h('p', { class: 'fc-note' }, 'This server has no custom emojis yet.'));
    } else if (!res) {
      grid.append(h('p', { class: 'fc-note' }, "The server's emojis are unavailable."));
    } else {
      grid.append(h('p', { class: 'fc-note' }, 'No server emoji matches.'));
    }
    if (!q) {
      grid.append(...emojiGrid('Common', COMMON_EMOJI.map((e) => h('button', {
        type: 'button', class: 'fc-emoji-btn', 'aria-label': e, onclick: () => chooseEmoji(e),
      }, e))));
    }
  }
  function togglePicker() {
    if (pop.isOpen) closePicker(true);
    else openPicker();
  }
  function openPicker() {
    search.value = '';
    renderGrid();
    pop.open();
    pickBtn.setAttribute('aria-expanded', 'true');
    pop.place();
    search.focus({ preventScroll: true });
  }
  function closePicker(refocus) {
    if (!pop.isOpen) return;
    pop.close();
    pickBtn.setAttribute('aria-expanded', 'false');
    if (refocus) pickBtn.focus({ preventScroll: true });
  }
  search.addEventListener('input', () => {
    renderGrid();
    pop.place();
  });
  panel.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      closePicker(true);
      return;
    }
    const keys = ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'];
    if (!keys.includes(event.key) || event.target === search) {
      if (event.key === 'ArrowDown' && event.target === search) {
        event.preventDefault();
        panel.querySelector('.fc-emoji-btn')?.focus();
      }
      return;
    }
    const buttons = [...panel.querySelectorAll('.fc-emoji-btn')];
    const at = buttons.indexOf(event.target);
    if (at < 0) return;
    event.preventDefault();
    const columns = Math.max(1, buttons.filter((b) => b.offsetTop === buttons[0].offsetTop).length);
    let next = at;
    if (event.key === 'ArrowLeft') next = at - 1;
    else if (event.key === 'ArrowRight') next = at + 1;
    else if (event.key === 'ArrowDown') next = at + columns;
    else next = at - columns;
    if (next < 0) search.focus();
    else if (next < buttons.length) buttons[next].focus();
  });

  setText(text);

  return {
    body: root,
    inputs: [input],
    labelFor: input.id,
    // '' is not an emoji, so a nullable field sends null for it.
    getValue() {
      const v = text.trim();
      if (v === '' && (desc.nullable || (origNull && !touched))) return null;
      return v;
    },
    setValue(v) {
      origNull = v === null;
      touched = false;
      setText(typeof v === 'string' ? v : '');
    },
    setResources() {
      if (pop.isOpen) renderGrid();
    },
    destroy: () => closePicker(false),
  };
}

/* ------------------------------------------------------------------ */
/* object / objectList                                                  */
/* ------------------------------------------------------------------ */

function buildObject(desc, st) {
  let untouchedNull = st.value === null && desc.nullable === true;
  const source = isObject(st.value) ? st.value : {};
  const form = createForm({
    fields: desc.fields ?? [],
    values: source,
    ctx: st.childCtx(),
    idPrefix: st.id,
    onChange: () => {
      untouchedNull = false;
      st.changed();
    },
  });
  return {
    layout: 'fieldset',
    body: form.el,
    inputs: [],
    getValue: () => (untouchedNull ? null : form.getValues()),
    validate: () => form.validate(),
    setValue(v) {
      untouchedNull = v === null && desc.nullable === true;
      form.setValues(isObject(v) ? v : {});
    },
    routeError(sub, message) {
      form.addErrors([{ path: sub, message }]);
      return true;
    },
    clearErrors: () => form.setErrors([]),
    setResources: (r) => form.setResources(r),
    setDisabled: () => {},
    focusPath: (sub) => form.focusPath(sub),
    focus: () => form.focusPath(desc.fields?.[0]?.field ?? ''),
    destroy: () => form.destroy(),
  };
}

function itemSummary(fields, values) {
  const clip = (v) => {
    const s = String(v).replace(/\s+/g, ' ').trim();
    return s.length > 60 ? `${s.slice(0, 57)}…` : s;
  };
  const filled = (f) => values[f.field] !== null && values[f.field] !== undefined && values[f.field] !== '';
  for (const f of fields) {
    if (!['text', 'enum', 'url', 'emoji'].includes(f.type) || !filled(f)) continue;
    if (f.type === 'enum') return f.options?.find((o) => o.value === values[f.field])?.label ?? String(values[f.field]);
    return clip(values[f.field]);
  }
  const num = fields.find((f) => ['int', 'float'].includes(f.type) && filled(f));
  return num ? `${num.label}: ${values[num.field]}${num.unit ? ` ${num.unit}` : ''}` : '';
}

function buildObjectList(desc, st) {
  const itemLabel = desc.itemLabel ?? 'Item';
  const maxItems = desc.maxItems ?? BIG_LIST;
  const minItems = desc.minItems ?? 0;
  const fields = desc.itemFields ?? [];
  const entries = [];

  const list = h('ol', { class: 'fc-items' });
  const empty = h('p', { class: 'fc-empty' }, desc.emptyText ?? `No ${itemLabel.toLowerCase()}s yet.`);
  const live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
  const counter = h('span', { class: 'fc-counter' });
  const addBtn = h('button', { type: 'button', class: 'btn btn-secondary btn-sm', onclick: () => add() },
    icon('plus'), desc.addLabel ?? `Add ${itemLabel.toLowerCase()}`);

  function makeEntry(values, { open = true } = {}) {
    const key = nextKey();
    const entry = { key, open, errorMsg: null };
    entry.form = createForm({
      fields, values, ctx: st.childCtx(), idPrefix: `${st.id}-${key}`,
      onChange: () => {
        paintEntry(entry);
        st.changed();
      },
    });
    entry.bodyId = `${st.id}-${key}-body`;
    entry.title = h('span', { class: 'fc-item-title' });
    entry.summary = h('span', { class: 'fc-item-summary' });
    entry.toggle = h('button', {
      type: 'button', class: 'fc-item-toggle', 'aria-controls': entry.bodyId,
      onclick: () => {
        entry.open = !entry.open;
        paintEntry(entry);
      },
    }, icon('chevron'), entry.title, entry.summary);
    entry.up = h('button', { type: 'button', class: 'icon-btn fc-icon-btn', onclick: () => move(entry, -1) }, localIcon('up'));
    entry.down = h('button', { type: 'button', class: 'icon-btn fc-icon-btn', onclick: () => move(entry, 1) }, localIcon('down'));
    entry.remove = h('button', {
      type: 'button', class: 'icon-btn fc-icon-btn fc-danger', onclick: () => removeEntry(entry),
    }, localIcon('trash'));
    entry.body = h('div', { class: 'fc-item-body', id: entry.bodyId }, entry.form.el);
    entry.err = h('p', { class: 'fc-error', role: 'alert', hidden: true });
    entry.el = h('li', { class: 'fc-item' },
      h('div', { class: 'fc-item-head' }, entry.toggle,
        h('div', { class: 'fc-item-actions' }, entry.up, entry.down, entry.remove)),
      entry.body, entry.err);
    return entry;
  }

  function paintEntry(entry) {
    const i = entries.indexOf(entry);
    const name = `${itemLabel} ${i + 1}`;
    entry.title.textContent = name;
    entry.summary.textContent = itemSummary(fields, entry.form.getValues());
    entry.toggle.setAttribute('aria-expanded', String(entry.open));
    entry.body.hidden = !entry.open;
    entry.el.classList.toggle('is-collapsed', !entry.open);
    entry.up.setAttribute('aria-label', `Move ${name} up`);
    entry.down.setAttribute('aria-label', `Move ${name} down`);
    entry.remove.setAttribute('aria-label', `Remove ${name}`);
    entry.up.disabled = i === 0;
    entry.down.disabled = i === entries.length - 1;
    entry.remove.disabled = entries.length <= minItems;
  }

  function paintAll() {
    entries.forEach(paintEntry);
    empty.hidden = entries.length > 0;
    addBtn.disabled = entries.length >= maxItems;
    counter.textContent = `${entries.length} / ${maxItems}`;
    counter.classList.toggle('is-near', entries.length >= maxItems);
  }

  function add() {
    if (entries.length >= maxItems) return;
    const values = Object.fromEntries(fields.map((f) => [f.field, defaultFor(f)]));
    const entry = makeEntry(values);
    entries.push(entry);
    list.append(entry.el);
    paintAll();
    live.textContent = `${itemLabel} ${entries.length} added.`;
    st.changed();
    entry.el.querySelector('.fc-item-body input, .fc-item-body select, .fc-item-body textarea, .fc-item-body button')?.focus();
  }

  function removeEntry(entry) {
    if (entries.length <= minItems) return;
    const i = entries.indexOf(entry);
    entries.splice(i, 1);
    entry.form.destroy();
    entry.el.remove();
    paintAll();
    live.textContent = `${itemLabel} ${i + 1} removed.`;
    st.changed();
    (entries[Math.min(i, entries.length - 1)]?.toggle ?? addBtn).focus();
  }

  function move(entry, dir) {
    const i = entries.indexOf(entry);
    const j = i + dir;
    if (j < 0 || j >= entries.length) return;
    [entries[i], entries[j]] = [entries[j], entries[i]];
    list.replaceChildren(...entries.map((e) => e.el));
    paintAll();
    live.textContent = `${itemLabel} moved to position ${j + 1} of ${entries.length}.`;
    st.changed();
    const same = dir < 0 ? entry.up : entry.down;
    (same.disabled ? entry.toggle : same).focus();
  }

  function setItems(values) {
    for (const e of entries) e.form.destroy();
    entries.length = 0;
    const source = Array.isArray(values) ? values.filter(isObject).slice(0, maxItems) : [];
    // Long lists start collapsed so the page stays scannable.
    for (const v of source) entries.push(makeEntry(v, { open: source.length <= 3 }));
    list.replaceChildren(...entries.map((e) => e.el));
    paintAll();
  }
  setItems(st.value);

  const body = h('div', { class: 'fc-objectlist' }, empty, list,
    h('div', { class: 'fc-meta' }, addBtn, h('span', { class: 'fc-grow' }), counter), live);
  return {
    body,
    group: body,
    inputs: [],
    getValue: () => entries.map((e) => e.form.getValues()),
    validate() {
      return entries.flatMap((entry, i) => {
        const problems = entry.form.validate();
        if (problems.length) {
          entry.open = true; // a collapsed item must not hide the field that stops the save
          paintEntry(entry);
        }
        return problems.map((p) => ({ path: p.path ? `${i}.${p.path}` : String(i), message: p.message }));
      });
    },
    setValue: setItems,
    routeError(sub, message) {
      const [first, ...rest] = sub.split('.');
      const i = Number.parseInt(first, 10);
      const entry = Number.isInteger(i) ? entries[i] : null;
      if (!entry) return false;
      if (rest.length) {
        entry.open = true;
        paintEntry(entry);
        entry.form.addErrors([{ path: rest.join('.'), message }]);
      } else {
        entry.err.textContent = message;
        entry.err.hidden = false;
        entry.open = true;
        paintEntry(entry);
      }
      return true;
    },
    clearErrors() {
      for (const e of entries) {
        e.form.setErrors([]);
        e.err.hidden = true;
        e.err.textContent = '';
      }
    },
    setResources(r) {
      for (const e of entries) e.form.setResources(r);
    },
    focusPath(sub) {
      const [first, ...rest] = sub.split('.');
      const entry = entries[Number.parseInt(first, 10)];
      if (!entry) return;
      entry.open = true;
      paintEntry(entry);
      entry.form.focusPath(rest.join('.'));
    },
    focus: () => (entries[0]?.toggle ?? addBtn).focus(),
    destroy() {
      for (const e of entries) e.form.destroy();
    },
  };
}

/* ------------------------------------------------------------------ */
/* Builders by descriptor type                                          */
/* ------------------------------------------------------------------ */

const BUILDERS = {
  boolean: buildBoolean,
  int: buildNumber,
  float: buildNumber,
  text: buildText,
  url: buildText,
  enum: buildEnum,
  multiEnum: buildMultiEnum,
  color: buildColor,
  timezone: (desc, st) => buildChooser(desc, st, timezoneConfig(), false),
  channel: (desc, st) => buildChooser(desc, st, channelConfig(desc, st, false), false),
  channels: (desc, st) => buildChooser(desc, st, channelConfig(desc, st, true), true),
  role: (desc, st) => buildChooser(desc, st, roleConfig(desc, st, false), false),
  roles: (desc, st) => buildChooser(desc, st, roleConfig(desc, st, true), true),
  user: buildUser,
  stringList: buildStringList,
  datetime: buildDatetime,
  time: buildTime,
  emoji: buildEmoji,
  object: buildObject,
  objectList: buildObjectList,
};

/* ------------------------------------------------------------------ */
/* Form                                                                 */
/* ------------------------------------------------------------------ */

/**
 * A form for a list of descriptors. `getValues()` returns `{ field: value }` for every field in wire shapes.
 * @param {{ fields: object[], values?: object, ctx?: object|null, onChange?: (field: string, value: unknown) => void,
 *   disabled?: boolean, idPrefix?: string }} opts
 * @returns {{ el: HTMLElement, getValue: (field: string) => unknown, getValues: () => object,
 *   setValues: (values: object) => void, setErrors: (errors: { path: string, message: string }[]) => void,
 *   addErrors: (errors: { path: string, message: string }[]) => void,
 *   validate: (only?: string[]) => { path: string, message: string }[],
 *   focusPath: (path: string) => void, setResources: (resources: object|null) => void,
 *   setDisabled: (disabled: boolean) => void, destroy: () => void }}
 */
export function createForm({ fields = [], values = {}, ctx = null, onChange, disabled = false, idPrefix = 'f' } = {}) {
  const controls = new Map();
  const formError = h('p', { class: 'fc-error fc-form-error', role: 'alert', hidden: true });
  const el = h('div', { class: 'fc-form' }, formError);

  for (const desc of fields) {
    const initial = values && values[desc.field] !== undefined ? values[desc.field] : undefined;
    const control = createField(desc, {
      value: initial,
      ctx,
      idPrefix,
      disabled,
      onChange: (v) => {
        paintVisibility();
        onChange?.(desc.field, v);
      },
    });
    controls.set(desc.field, control);
    el.append(control.el);
  }

  // `showWhen: { field, in?: [...], notIn?: [...] }` hides a field while a sibling field's value does not match
  // (e.g. a rule's threshold while its action is off). A hidden field keeps its value. A field whose sibling is itself
  // hidden is hidden too (a panel template hides the role mode, and with it the mode's role limit).
  const conditional = fields.filter((d) => d.showWhen && typeof d.showWhen.field === 'string' && controls.has(d.showWhen.field));
  function paintVisibility() {
    const hidden = new Set();
    for (let pass = 0, changed = true; changed && pass <= conditional.length; pass++) {
      changed = false;
      for (const desc of conditional) {
        const { field, in: oneOf, notIn } = desc.showWhen;
        const value = controls.get(field).getValue();
        const shown = !hidden.has(field)
          && (!Array.isArray(oneOf) || oneOf.includes(value)) && (!Array.isArray(notIn) || !notIn.includes(value));
        if (shown === hidden.has(desc.field)) {
          if (shown) hidden.delete(desc.field);
          else hidden.add(desc.field);
          changed = true;
        }
      }
    }
    for (const desc of conditional) controls.get(desc.field).el.hidden = hidden.has(desc.field);
  }
  paintVisibility();

  function clearErrors() {
    for (const control of controls.values()) control.setError(null);
    formError.hidden = true;
    formError.textContent = '';
  }

  // Routes each error by the first segment of its path; the first message per path wins. Nothing is cleared.
  function addErrors(errors) {
    const seen = new Set();
    const unrouted = [];
    for (const error of errors ?? []) {
      const path = String(error?.path ?? '');
      const message = String(error?.message ?? '');
      if (!message || seen.has(path)) continue;
      seen.add(path);
      const [first, ...rest] = path.split('.');
      const control = controls.get(first);
      if (control) {
        control.el.hidden = false; // an error is never hidden behind showWhen
        control.setError(message, rest.join('.'));
      } else unrouted.push(message);
    }
    if (unrouted.length) {
      formError.textContent = [formError.textContent, ...unrouted].filter(Boolean).join(' ');
      formError.hidden = false;
    }
  }

  return {
    el,
    getValue: (field) => controls.get(field)?.getValue(),
    getValues: () => Object.fromEntries([...controls].map(([field, control]) => [field, control.getValue()])),
    // The problems a save must not skip: entries a control shows but getValue() does not hold (an emptied number).
    // Marks each on its field and returns [{ path, message }]. Fields hidden by showWhen are not looked at, and `only`
    // (field names) limits it to the fields the save sends, so a stored value that is out of range never blocks another edit.
    validate(only = null) {
      const problems = [];
      for (const [field, control] of controls) {
        if (control.el.hidden || (only && !only.includes(field))) continue;
        for (const p of control.validate()) problems.push({ path: p.path ? `${field}.${p.path}` : field, message: p.message });
      }
      return problems;
    },
    setValues(next) {
      if (!next) return;
      for (const [field, control] of controls) {
        if (Object.hasOwn(next, field)) control.setValue(next[field]);
      }
      paintVisibility();
    },
    setErrors(errors) {
      clearErrors();
      addErrors(errors);
    },
    addErrors,
    focusPath(path) {
      const [first, ...rest] = String(path ?? '').split('.');
      const control = controls.get(first);
      if (!control) return;
      control.focusPath(rest.join('.'));
      control.el.scrollIntoView({ block: 'center', behavior: prefersReducedMotion() ? 'auto' : 'smooth' });
    },
    setResources(resources) {
      for (const control of controls.values()) control.setResources(resources);
    },
    setDisabled(next) {
      for (const control of controls.values()) control.setDisabled(next);
    },
    destroy() {
      for (const control of controls.values()) control.destroy();
    },
  };
}
