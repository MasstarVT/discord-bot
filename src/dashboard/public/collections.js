// STUB (replaced by the collections implementation): mountCollection per the dashboard contract.
import { h } from './lib.js';

export function mountCollection(container, { collection } = {}) {
  container.append(h('p', { class: 'stub-collection' }, `${collection?.label ?? 'List'}: coming soon.`));
  return { destroy: () => {}, refresh: () => {} };
}
