// Component handler registries (01-foundation §8.4). One map per component kind, keyed by the
// customId prefix ("action" of "action:param1:param2"). The first registration of a prefix wins:
// a different second handler is refused and logged, so two packages can never silently overwrite
// each other (and the legacy `registerHandlers(client)` hooks of command files stay safe).
import logger from '../utils/logger.js';

export class HandlerMap extends Map {
  /** @param {'button'|'select'|'modal'} kind */
  constructor(kind) {
    super();
    this.kind = kind;
  }

  /**
   * First registration wins; re-registering the same function is a no-op.
   * @param {string} key customId prefix
   * @param {Function} handler
   * @returns {this}
   */
  set(key, handler) {
    if (super.has(key) && super.get(key) !== handler) {
      logger.error(`Duplicate ${this.kind} handler for customId prefix "${key}" ignored`);
      return this;
    }
    return super.set(key, handler);
  }
}

/** Replaces client.buttonHandlers / selectHandlers / modalHandlers with guarded maps. */
export function createHandlerMaps(client) {
  client.buttonHandlers = new HandlerMap('button');
  client.selectHandlers = new HandlerMap('select');
  client.modalHandlers  = new HandlerMap('modal');
}

// Module-level so that a second registerCoreComponents() call re-registers the same function.
const noopHandler = (interaction) => interaction.deferUpdate().catch(() => {});

/** Registers the persistent components the core itself owns (`noop`: a button that does nothing). */
export function registerCoreComponents(client) {
  client.buttonHandlers.set('noop', noopHandler);
}
