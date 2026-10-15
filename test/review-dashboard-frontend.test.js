// Regression tests for the dashboard's browser code (src/dashboard/public). There is no DOM library in the project,
// so these run the real modules in plain Node: the API client against a stubbed fetch, the toast host against a tiny
// fake document, and source-level checks for behaviour that lives inside the page closures.
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const PUBLIC = join(ROOT, 'src/dashboard/public');
const read = (file) => readFileSync(join(PUBLIC, file), 'utf8');

// Imported before any fake document exists: app.js starts itself only when `document` is defined at import time.
const appModule = await import('../src/dashboard/public/app.js');

// ── A tiny fake document: just enough for h(), icon() and toast() ─────────────────────────────────────────────────

class FakeNode {
  constructor() {
    this.parentNode = null;
    this.childNodes = [];
  }

  get parentElement() {
    return this.parentNode;
  }

  get isConnected() {
    for (let n = this; n; n = n.parentNode) if (n === globalThis.document) return true;
    return false;
  }

  append(...nodes) {
    for (const n of nodes) {
      const node = typeof n === 'string' ? new FakeText(n) : n;
      node.remove();
      node.parentNode = this;
      this.childNodes.push(node);
    }
  }

  remove() {
    const p = this.parentNode;
    if (!p) return;
    p.childNodes.splice(p.childNodes.indexOf(this), 1);
    this.parentNode = null;
  }
}

class FakeText extends FakeNode {
  constructor(text) {
    super();
    this.textContent = text;
  }
}

class FakeElement extends FakeNode {
  constructor(tag) {
    super();
    this.tagName = tag.toUpperCase();
    this.attrs = new Map();
    this.listeners = new Map();
    this.classes = new Set();
    this.dataset = {};
    this.classList = {
      add: (...c) => c.forEach((x) => this.classes.add(x)),
      remove: (...c) => c.forEach((x) => this.classes.delete(x)),
      toggle: (c, on) => (on ?? !this.classes.has(c) ? this.classes.add(c) : this.classes.delete(c)),
      contains: (c) => this.classes.has(c),
    };
  }

  set className(v) {
    this.classes = new Set(String(v).split(/\s+/).filter(Boolean));
  }

  get className() {
    return [...this.classes].join(' ');
  }

  set textContent(v) {
    this.childNodes = [];
    if (v) this.append(String(v));
  }

  get textContent() {
    return this.childNodes.map((n) => n.textContent).join('');
  }

  setAttribute(k, v) {
    this.attrs.set(k, String(v));
  }

  getAttribute(k) {
    return this.attrs.has(k) ? this.attrs.get(k) : null;
  }

  hasAttribute(k) {
    return this.attrs.has(k);
  }

  removeAttribute(k) {
    this.attrs.delete(k);
  }

  addEventListener(type, fn) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }

  /** Runs the listeners of an event, as the browser would for the user doing it. */
  fire(type, event = {}) {
    for (const fn of this.listeners.get(type) ?? []) fn({ target: this, preventDefault() {}, ...event });
  }

  querySelector() {
    return null;
  }

  contains(node) {
    for (let n = node; n; n = n.parentNode) if (n === this) return true;
    return false;
  }

  replaceChildren(...nodes) {
    for (const n of [...this.childNodes]) n.remove();
    this.append(...nodes);
  }

  toggleAttribute(k, on) {
    if (on ?? !this.attrs.has(k)) this.attrs.set(k, '');
    else this.attrs.delete(k);
  }

  focus() {}

  scrollIntoView() {}

  showModal() {}

  close() {}

  elements() {
    return this.childNodes.flatMap((n) => (n instanceof FakeElement ? [n, ...n.elements()] : []));
  }

  // Only `tag[attr]` selectors, which is all lib.js asks for.
  querySelectorAll(selector) {
    const m = /^([a-z]+)\[([a-z-]+)\]$/.exec(selector);
    assert.ok(m, `unsupported selector ${selector}`);
    return this.elements().filter((e) => e.tagName === m[1].toUpperCase() && e.hasAttribute(m[2]));
  }
}

function installFakeDom() {
  const document = new FakeElement('#document');
  document.body = new FakeElement('body');
  document.append(document.body);
  document.createElement = (tag) => new FakeElement(tag);
  document.createElementNS = (ns, tag) => new FakeElement(tag);
  document.createTextNode = (text) => new FakeText(text);
  document.querySelector = () => null;
  globalThis.document = document;
  globalThis.Node = FakeNode;
  return document;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ── lib.api: the contract with server.js ─────────────────────────────────────────────────────────────────────────

describe('lib.api', () => {
  let lib;
  let calls;
  const realFetch = globalThis.fetch;

  before(async () => {
    lib = await import('../src/dashboard/public/lib.js');
  });
  after(() => {
    globalThis.fetch = realFetch;
  });

  const stub = (response) => {
    calls = [];
    globalThis.fetch = async (path, init) => {
      calls.push({ path, init });
      if (response instanceof Error) throw response;
      return response;
    };
  };
  const json = (status, body, headers = {}) => ({
    ok: status >= 200 && status < 300,
    status,
    headers: { get: (k) => headers[k] ?? null },
    json: async () => body,
  });

  test('writes carry the CSRF header and a JSON body; reads carry neither', async () => {
    stub(json(200, { ok: true }));
    await lib.api('/api/guilds/1/settings');
    assert.equal(calls[0].init.method, 'GET');
    assert.equal(calls[0].init.headers['X-Dashboard'], undefined);
    assert.equal(calls[0].init.body, undefined);
    assert.equal(calls[0].init.credentials, 'same-origin');

    for (const method of ['POST', 'PATCH', 'DELETE']) {
      stub(json(200, { ok: true }));
      await lib.api('/x', { method, body: { changes: { a: 1 } } });
      assert.equal(calls[0].init.headers['X-Dashboard'], '1', method);
      assert.equal(calls[0].init.headers['Content-Type'], 'application/json', method);
      assert.equal(calls[0].init.credentials, 'same-origin', method);
    }
    stub(json(200, {}));
    await lib.api('/x', { method: 'DELETE' });
    assert.equal(calls[0].init.body, '{}', 'a bodiless write still sends valid JSON');
  });

  test('ids stay strings on the wire (snowflakes above 2^53)', async () => {
    stub(json(200, {}));
    await lib.api('/x', { method: 'PATCH', body: { changes: { welcomeChannelId: '123456789012345678' } } });
    assert.equal(JSON.parse(calls[0].init.body).changes.welcomeChannelId, '123456789012345678');
    assert.ok(calls[0].init.body.includes('"123456789012345678"'));
  });

  test('error answers keep the status, code, message, path and retry delay', async () => {
    stub(json(400, { error: 'invalid_request', message: 'Bad value.', path: 'a.b' }));
    await assert.rejects(lib.api('/x', { method: 'PATCH', body: {} }), (err) => {
      assert.ok(err instanceof lib.ApiError);
      assert.equal(err.status, 400);
      assert.equal(err.code, 'invalid_request');
      assert.equal(err.serverMessage, 'Bad value.');
      assert.equal(err.body.path, 'a.b');
      return true;
    });
    stub(json(429, { error: 'rate_limited', message: 'Slow.', retryAfter: 7 }));
    await assert.rejects(lib.api('/x'), (err) => err.retryAfter === 7 && err.status === 429);
    stub(json(429, null, { 'Retry-After': '3' }));
    await assert.rejects(lib.api('/x'), (err) => err.retryAfter === 3);
  });

  test('a network failure is status 0, a 204 is null', async () => {
    stub(new TypeError('failed to fetch'));
    await assert.rejects(lib.api('/x'), (err) => err.status === 0 && err.code === 'network');
    stub({ ok: true, status: 204, headers: { get: () => null }, json: async () => { throw new Error('no body'); } });
    assert.equal(await lib.api('/x', { method: 'DELETE' }), null);
  });
});

// ── lib.toast: a toast must be visible above a modal dialog ──────────────────────────────────────────────────────

describe('lib.toast', () => {
  test('lives inside the open modal dialog, and goes back to the page when none is open', async () => {
    const document = installFakeDom();
    const lib = await import('../src/dashboard/public/lib.js');

    lib.toast('first', { ms: 1 });
    const root = document.body.elements().find((e) => e.classes.has('toasts'));
    assert.ok(root, 'the toast box was created');
    assert.equal(root.parentElement, document.body);

    // A modal <dialog> is drawn in the top layer: a toast left in <body> would be hidden behind it.
    const dialog = document.createElement('dialog');
    dialog.setAttribute('open', '');
    document.body.append(dialog);
    lib.toast('second', { ms: 1 });
    assert.equal(root.parentElement, dialog, 'the box moved into the dialog');

    // A repeated (keyed) toast re-hosts the box too.
    dialog.remove();
    const second = document.createElement('dialog');
    second.setAttribute('open', '');
    document.body.append(second);
    lib.toast('keyed', { key: 'k', ms: 1 });
    lib.toast('keyed again', { key: 'k', ms: 1 });
    const live = document.body.elements().find((e) => e.classes.has('toasts'));
    assert.equal(live.parentElement, second);
    assert.ok(live.textContent.includes('keyed again'));

    second.remove();
    lib.toast('third', { ms: 1 });
    const last = document.body.elements().find((e) => e.classes.has('toasts'));
    assert.equal(last.parentElement, document.body, 'back in the page once the dialog is gone');
    await sleep(300); // let the toasts dismiss themselves
  });
});

// ── Behaviour that lives inside the page closures (checked in the source) ────────────────────────────────────────

describe('page behaviour', () => {
  const app = read('app.js');
  const body = (name, source) => {
    const start = source.indexOf(name);
    assert.ok(start >= 0, `${name} exists`);
    return source.slice(start, start + 1400);
  };

  test('the Refresh / Retry reload skips the server cache (?fresh=1) and reports failures', () => {
    const fn = body('function reloadResources()', app);
    assert.match(fn, /\/resources\?fresh=1/);
    assert.match(fn, /toast\(describeError\(err\)\.message, \{ kind: 'error', key: 'resources-error'/);
    // the first load of a page may use the cache
    assert.match(app, /api\(`\$\{base\}\/resources`\)/);
  });

  test('log out asks about unsaved work before the session is revoked, and does not ask twice', () => {
    const fn = body('async function logout(btn)', app);
    assert.ok(fn.indexOf('hasUnsavedWork()') >= 0 && fn.indexOf('hasUnsavedWork()') < fn.indexOf("api('/auth/logout'"));
    assert.ok(fn.indexOf('leaving = true') > fn.indexOf("api('/auth/logout'"));
    assert.match(app, /beforeunload[\s\S]{0,120}!leaving && hasUnsavedWork\(\)/);
  });

  test('an open record editor counts as unsaved work', () => {
    assert.match(read('collections.js'), /isDirty: \(\) => Boolean\(editor && editor\.isDirty\(\)\)/);
    assert.match(app, /isDirty: \(\) => dirty\.size > 0 \|\| handles\.some\(/);
  });
});

// ── GuildStore: the switches the overview and the module pages paint ─────────────────────────────────────────────

describe('GuildStore', () => {
  const realFetch = globalThis.fetch;
  const realSetTimeout = globalThis.setTimeout;
  let patches;

  before(() => {
    installFakeDom(); // a save shows a "Saved" toast
    // The "Saved" toast and its dismissal use timers of seconds: run them at once.
    globalThis.setTimeout = (fn, ms, ...args) => realSetTimeout(fn, Math.min(ms ?? 0, 5), ...args);
  });
  after(async () => {
    await sleep(30);
    globalThis.fetch = realFetch;
    globalThis.setTimeout = realSetTimeout;
  });

  /** The PATCH answers with the whole settings page, like the server does. */
  const answerWith = (values, status = 200) => {
    patches = [];
    globalThis.fetch = async (path, init) => {
      patches.push({ path, body: JSON.parse(init.body) });
      return { ok: status === 200, status, headers: { get: () => null }, json: async () => (status === 200 ? { values } : { error: 'save_failed', message: 'No.' }) };
    };
  };
  const flipped = async (store, field) => {
    store.toggle(field, false);
    clearTimeout(store.timer); // flush now instead of after the debounce
    store.timer = 0;
    await store.flush();
  };

  test('a settings GET that was sent before a toggle was saved does not undo the switch', async () => {
    answerWith({ modEnabled: true, xpEnabled: false });
    const store = new appModule.GuildStore('1');
    store.load({ modEnabled: false, xpEnabled: false });

    const since = store.mark(); // the page asks for the settings ...
    await flipped(store, 'modEnabled'); // ... the admin flips a switch, and the PATCH is answered ...
    assert.equal(patches.length, 1);
    assert.equal(store.get('modEnabled', false), true);

    // ... and only then does the older GET answer, still saying "off".
    const shown = store.load({ modEnabled: false, xpEnabled: false, xpMin: 15 }, since);
    assert.equal(store.get('modEnabled', false), true, 'the stale switch is ignored');
    assert.equal(store.confirmed.xpMin, 15, 'fields nothing was written to still load');
    // The module page builds its forms (which hold the same switches) from what load() returns, not from the raw answer.
    assert.equal(shown.modEnabled, true);
    assert.equal(shown.xpMin, 15);
    assert.notEqual(shown, store.confirmed, 'a copy');
  });

  test('a GET sent after the toggle was saved, or one that has no mark, is taken as it is', async () => {
    answerWith({ modEnabled: true });
    const store = new appModule.GuildStore('2');
    store.load({ modEnabled: false });
    await flipped(store, 'modEnabled');

    store.load({ modEnabled: false }, store.mark());
    assert.equal(store.get('modEnabled', true), false, 'asked after the write, so it is newer');
    store.load({ modEnabled: true });
    assert.equal(store.get('modEnabled', false), true, 'no mark: replaces everything, as before');
  });

  test('settings saved by a form count as a write too; a failed save does not', async () => {
    const store = new appModule.GuildStore('3');
    store.load({ xpEnabled: false, modEnabled: false });
    const since = store.mark();
    store.confirm({ xpEnabled: true });
    store.load({ xpEnabled: false, modEnabled: false }, since);
    assert.equal(store.get('xpEnabled', false), true);

    answerWith({}, 500);
    const again = store.mark();
    await flipped(store, 'modEnabled');
    assert.equal(store.get('modEnabled', true), false, 'rolled back');
    store.load({ modEnabled: true }, again);
    assert.equal(store.get('modEnabled', false), true, 'nothing was written, so the GET is the newest word');
  });
});

// ── Ctrl+S ───────────────────────────────────────────────────────────────────────────────────────────────────────

describe('Ctrl+S', () => {
  const key = (extra = {}) => ({ key: 's', ctrlKey: true, prevented: 0, preventDefault() { this.prevented += 1; }, ...extra });
  const view = (extra = {}) => {
    const calls = [];
    return { calls, save: () => calls.push('page'), ...extra };
  };

  test('saves the page when no dialog is open', () => {
    const v = view();
    const event = key();
    assert.equal(appModule.handleSaveShortcut(event, v), true);
    assert.deepEqual(v.calls, ['page']);
    assert.equal(event.prevented, 1);
    assert.equal(appModule.handleSaveShortcut(key({ key: 'S', ctrlKey: false, metaKey: true }), v), true, 'Cmd+S and a capital S too');
    const open = view({ dialogOpen: () => false, saveDialog: () => assert.fail('no dialog is open') });
    appModule.handleSaveShortcut(key(), open);
    assert.deepEqual(open.calls, ['page']);
  });

  test('with a dialog open it saves that dialog, never the page behind it', () => {
    const v = view({ dialogOpen: () => true, saveDialog: () => v.calls.push('dialog') });
    const event = key();
    assert.equal(appModule.handleSaveShortcut(event, v), true);
    assert.deepEqual(v.calls, ['dialog']);
    assert.equal(event.prevented, 1);
  });

  test('with a dialog open that cannot save, nothing is saved (and the browser still does not open "save page")', () => {
    const v = view({ dialogOpen: () => true });
    const event = key();
    assert.equal(appModule.handleSaveShortcut(event, v), true);
    assert.deepEqual(v.calls, []);
    assert.equal(event.prevented, 1);
  });

  test('other keys, and pages without a save, are left alone', () => {
    const v = view();
    for (const event of [key({ ctrlKey: false }), key({ key: 'a' }), key({ altKey: true }), key({ shiftKey: true })]) {
      assert.equal(appModule.handleSaveShortcut(event, v), false);
      assert.equal(event.prevented, 0);
    }
    assert.equal(appModule.handleSaveShortcut(key(), null), false);
    assert.equal(appModule.handleSaveShortcut(key(), {}), false);
    assert.deepEqual(v.calls, []);
  });
});

// ── Number fields: what is typed versus what a save would send ───────────────────────────────────────────────────

describe('forms: a number box that was emptied or typed out of range blocks the save', () => {
  let forms;
  before(async () => {
    installFakeDom();
    forms = await import('../src/dashboard/public/forms.js');
  });

  const type = (form, text, index = 0) => {
    const input = form.el.elements().filter((e) => e.tagName === 'INPUT')[index];
    input.value = text;
    input.fire('input');
    return input;
  };
  const shown = (form) => form.el.elements().filter((e) => e.classes.has('fc-error') && !e.classes.has('fc-form-error'))
    .map((e) => e.textContent).filter(Boolean);
  const xp = { field: 'xpMin', type: 'int', label: 'Min XP', min: 1, max: 100, default: 15 };

  test('an emptied required number keeps its last value in getValues(), but validate() stops on it with the field message', () => {
    const form = forms.createForm({ fields: [xp, { field: 'xpMax', type: 'int', label: 'Max XP', min: 1, max: 200, default: 30 }], values: { xpMin: 20, xpMax: 30 } });
    assert.deepEqual(form.validate(), []);

    type(form, '');
    assert.equal(form.getValue('xpMin'), 20, 'the stale value a save would send');
    assert.deepEqual(form.validate(), [{ path: 'xpMin', message: 'Enter a number.' }]);
    assert.deepEqual(shown(form), ['Enter a number.']);

    type(form, '500');
    assert.deepEqual(form.validate(), [{ path: 'xpMin', message: 'Must be at most 100.' }]);
    type(form, '0');
    assert.deepEqual(form.validate().map((p) => p.message), ['Must be at least 1.']);
    type(form, '1.5');
    assert.deepEqual(form.validate().map((p) => p.message), ['Must be a whole number.']);

    type(form, '42');
    assert.deepEqual(form.validate(), []);
    assert.equal(form.getValue('xpMin'), 42);
    assert.deepEqual(shown(form), []);
  });

  test('only the fields the save sends are checked; hidden fields and optional empty numbers are not', () => {
    const optional = { field: 'cap', type: 'int', label: 'Cap', nullable: true, min: 0, max: 9 };
    const hidden = { field: 'step', type: 'int', label: 'Step', min: 1, max: 9, default: 3, showWhen: { field: 'xpMin', in: [999] } };
    const form = forms.createForm({ fields: [xp, optional, hidden], values: { xpMin: 20, cap: 4, step: 3 } });
    type(form, '', 0);
    type(form, '', 1);
    type(form, '', 2);
    assert.deepEqual(form.validate().map((p) => p.path), ['xpMin'], 'the optional box may be empty; the hidden one is not looked at');
    assert.deepEqual(form.validate(['cap']), [], 'xpMin is not part of this save');
    assert.deepEqual(form.validate(['xpMin']).map((p) => p.path), ['xpMin']);
  });

  test('inside a list of items the path names the item and the field, and a collapsed item is opened', () => {
    const rewards = {
      field: 'rewards', type: 'objectList', label: 'Rewards', itemLabel: 'Reward',
      itemFields: [{ field: 'level', type: 'int', label: 'Level', min: 1, max: 100, default: 1 }],
    };
    const form = forms.createForm({ fields: [rewards], values: { rewards: [{ level: 5 }, { level: 7 }, { level: 9 }, { level: 11 }] } });
    assert.deepEqual(form.validate(), []);
    type(form, '', 2);
    assert.deepEqual(form.validate(), [{ path: 'rewards.2.level', message: 'Enter a number.' }]);
    const toggles = form.el.elements().filter((e) => e.classes.has('fc-item-toggle'));
    assert.equal(toggles[2].getAttribute('aria-expanded'), 'true', 'the item with the bad box is open');
  });
});

// ── Dialogs of a list: leaving the page closes the questions too ─────────────────────────────────────────────────

describe('collections: a question dialog is closed with the others when the page is left', () => {
  let collections;
  let document;
  before(async () => {
    document = installFakeDom();
    collections = await import('../src/dashboard/public/collections.js');
  });
  const dialogs = () => document.body.elements().filter((e) => e.tagName === 'DIALOG');
  const buttons = () => document.body.elements().filter((e) => e.tagName === 'BUTTON');

  test('a tracked question is in the set while it is open, and closing the set answers it "no"', async () => {
    const open = new Set();
    const answer = collections.confirmModal({ title: 'Discard unsaved changes?', message: 'x', danger: true, track: open });
    assert.equal(open.size, 1);
    assert.equal(dialogs().length, 1);

    for (const modal of [...open]) modal.close({ restoreFocus: false }); // what leaving the page does
    open.clear();
    assert.equal(await answer, false);
    assert.equal(dialogs().length, 0, 'no dialog is left on the page');
  });

  test('answering the question takes it out of the set again', async () => {
    const open = new Set();
    for (const [label, expected] of [['Keep editing', false], ['Discard', true]]) {
      const answer = collections.confirmModal({
        title: 'Discard unsaved changes?', message: 'x', confirmLabel: 'Discard', cancelLabel: 'Keep editing', track: open,
      });
      assert.equal(open.size, 1);
      buttons().find((b) => b.textContent === label).fire('click');
      assert.equal(await answer, expected, label);
      assert.equal(open.size, 0);
      assert.equal(dialogs().length, 0);
    }
  });

  test('without a set it works as before', async () => {
    const answer = collections.confirmModal({ title: 'Delete?', message: 'x' });
    buttons().find((b) => b.textContent === 'Confirm').fire('click');
    assert.equal(await answer, true);
  });
});

// ── A record editor over a list (mountCollection) ────────────────────────────────────────────────────────────────

describe('collections: the editor dialog', () => {
  const realFetch = globalThis.fetch;
  const realSetTimeout = globalThis.setTimeout;
  let collections;
  let document;
  let requests;
  before(async () => {
    document = installFakeDom();
    collections = await import('../src/dashboard/public/collections.js');
    // A save shows a toast that stays for seconds: run its timers at once.
    globalThis.setTimeout = (fn, ms, ...args) => realSetTimeout(fn, Math.min(ms ?? 0, 5), ...args);
  });
  after(async () => {
    await sleep(30);
    globalThis.fetch = realFetch;
    globalThis.setTimeout = realSetTimeout;
  });

  const dialogs = () => document.body.elements().filter((e) => e.tagName === 'DIALOG');
  const byClass = (root, cls) => root.elements().filter((e) => e.classes.has(cls));
  const settle = () => sleep(5);
  const collection = {
    key: 'things', label: 'Things', singular: 'thing', limit: 10, titleField: 'id',
    fields: [{ field: 'xp', type: 'int', label: 'XP', min: 1, max: 100, default: 5 }],
  };

  async function openCreateEditor() {
    requests = [];
    globalThis.fetch = async (path, init = {}) => {
      requests.push({ method: init.method ?? 'GET', path });
      const body = init.method === 'POST' ? { item: { id: 'thing1', xp: 50 } } : { items: [], limit: 10 };
      return { ok: true, status: init.method === 'POST' ? 201 : 200, headers: { get: () => null }, json: async () => body };
    };
    const container = document.createElement('div');
    document.body.append(container);
    const handle = collections.mountCollection(container, { collection, ctx: { guildId: '123456789012345678' } });
    await settle();
    byClass(container, 'cl-create')[0].fire('click');
    assert.equal(dialogs().length, 1, 'the editor is open');
    return { handle, container, dialog: dialogs()[0] };
  }
  const typeXp = (dialog, text) => {
    const input = dialog.elements().find((e) => e.tagName === 'INPUT');
    input.value = text;
    input.fire('input');
  };
  const posts = () => requests.filter((r) => r.method === 'POST');

  test('Ctrl+S in the editor saves it, but an emptied number box stops the save with the field message', async () => {
    const { handle, dialog } = await openCreateEditor();
    assert.equal(handle.dialogOpen(), true);

    typeXp(dialog, '');
    assert.equal(handle.saveEditor(), true, 'the editor has a Save button to press');
    assert.equal(posts().length, 0, 'nothing is sent while the box is empty');
    assert.equal(byClass(dialog, 'cl-dlg-status')[0].textContent, 'Please fix the highlighted field.');
    assert.ok(byClass(dialog, 'fc-error').some((e) => e.textContent === 'Enter a number.'));

    typeXp(dialog, '50');
    assert.equal(handle.saveEditor(), true);
    await settle();
    assert.equal(posts().length, 1, 'a valid number goes out');
    handle.destroy();
  });

  test('while the "discard changes?" question is on top, Ctrl+S saves nothing, and leaving the page closes the question too', async () => {
    const { handle } = await openCreateEditor();
    typeXp(dialogs()[0], '50'); // an unsaved edit: closing the editor asks first
    byClass(dialogs()[0], 'cl-dlg-close')[0].fire('click');
    assert.equal(dialogs().length, 2, 'the question is open over the editor');

    assert.equal(handle.dialogOpen(), true);
    assert.equal(handle.saveEditor(), false, 'the editor is not the dialog on top');
    assert.equal(posts().length, 0);

    handle.destroy(); // what navigating away (Back, a link) does
    assert.equal(dialogs().length, 0, 'no dialog is left behind');
    assert.equal(handle.dialogOpen(), false);
    await settle();
  });

  test('an editor with nothing changed has nothing to save: Ctrl+S does not close it', async () => {
    const { handle, container } = await openCreateEditor();
    handle.destroy();
    requests = [];
    globalThis.fetch = async () => ({
      ok: true, status: 200, headers: { get: () => null }, json: async () => ({ items: [{ id: 'thing1', xp: 5 }], limit: 10 }),
    });
    const list = document.createElement('div');
    document.body.append(list);
    const again = collections.mountCollection(list, { collection, ctx: { guildId: '123456789012345678' } });
    await settle();
    byClass(list, 'cl-row-main')[0].fire('click');
    assert.equal(dialogs().length, 1);
    assert.equal(again.saveEditor(), false, 'the Save button is disabled until something changes');
    assert.equal(dialogs().length, 1, 'and the editor stays open');
    again.destroy();
    assert.equal(container.parentNode, document.body);
  });
});
