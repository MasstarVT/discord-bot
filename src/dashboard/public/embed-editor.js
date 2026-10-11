// STUB (replaced by the embed editor implementation): createTemplateEditor / renderMessagePreview per the contract.
import { h } from './lib.js';

export function createTemplateEditor(desc, { value, onChange } = {}) {
  let current = value;
  const input = h('textarea', { class: 'stub-field', 'aria-label': desc.label, oninput: () => {
    try { current = JSON.parse(input.value); onChange?.(current); } catch { /* keep the last valid value */ }
  } });
  input.value = JSON.stringify(value ?? null, null, 2);
  return {
    el: h('label', { class: 'stub-row' }, h('span', {}, desc.label), input),
    getValue: () => current,
    setValue: (v) => { current = v; input.value = JSON.stringify(v ?? null, null, 2); },
    setError: () => {},
    setResources: () => {},
    focus: () => input.focus(),
    destroy: () => {},
  };
}

export function renderMessagePreview(template) {
  return h('pre', { class: 'stub-preview' }, JSON.stringify(template ?? null, null, 2));
}
