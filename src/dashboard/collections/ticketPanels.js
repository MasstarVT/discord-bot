// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `ticketPanels` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'ticketPanels',
  module: 'tickets',
  label: 'Ticket panels',
  singular: 'ticket panel',
  description: '',
  limit: LIMITS.TICKET_PANELS,
  titleField: 'id',
  canCreate: false,
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
