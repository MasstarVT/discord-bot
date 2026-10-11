// STUB (replaced by the form-controls implementation): createField / createForm per the dashboard contract.
import { h } from './lib.js';

export function createField(desc, { value, onChange } = {}) {
  let current = value;
  const input = h('textarea', { class: 'stub-field', 'aria-label': desc.label, oninput: () => {
    try { current = JSON.parse(input.value); onChange?.(current); } catch { /* keep the last valid value */ }
  } });
  input.value = JSON.stringify(value ?? null);
  const el = h('label', { class: 'stub-row' }, h('span', {}, desc.label), input);
  return {
    el,
    getValue: () => current,
    setValue: (v) => { current = v; input.value = JSON.stringify(v ?? null); },
    setError: () => {},
    setResources: () => {},
    focus: () => input.focus(),
    destroy: () => {},
  };
}

export function createForm({ fields, values = {}, ctx, onChange } = {}) {
  const controls = new Map(fields.map((f) => [f.field, createField(f, { value: values[f.field], ctx, onChange: (v) => onChange?.(f.field, v) })]));
  return {
    el: h('div', { class: 'stub-form' }, [...controls.values()].map((c) => c.el)),
    getValue: (field) => controls.get(field)?.getValue(),
    getValues: () => Object.fromEntries([...controls].map(([k, c]) => [k, c.getValue()])),
    setValues: (v) => { for (const [k, c] of controls) if (Object.hasOwn(v, k)) c.setValue(v[k]); },
    setErrors: () => {},
    focusPath: (path) => controls.get(String(path).split('.')[0])?.focus(),
    setResources: () => {},
    destroy: () => {},
  };
}
