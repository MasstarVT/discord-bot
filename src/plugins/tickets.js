// Ticketing plugin declaration (pkg-tickets.md §5): the channelDelete cleanup hook and the persistent panel components. Declaration
// only: the logic lives in services/ticketService.js and nothing runs at import.
import { ChannelType } from 'discord.js';
import { onChannelDelete, handleTicketButton, handleTicketSelect, handleTicketModal } from '../services/ticketService.js';

const RELEVANT = new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildCategory]);

export const plugin = {
  id: 'tickets',
  module: 'tickets',
  hooks: [
    // `always`: marking tickets DELETED and clearing dead references is cleanup and must happen while the module is off as well,
    // otherwise dead rows would keep counting against maxOpenPerUser.
    { event: 'channelDelete', id: 'tickets', always: true, filter: (ctx) => RELEVANT.has(ctx.channel?.type), run: onChannelDelete },
  ],
  components: [
    // Gated by the foundation wrapper (plugin.module = 'tickets'): while the module is off the buttons answer "The Ticketing module
    // is turned off on this server."
    { type: 'button', prefix: 'ticket', handler: handleTicketButton },
    { type: 'select', prefix: 'ticket-select', handler: handleTicketSelect },
    // NOT wrapped: the template form (`/ticket-panel message`) must work while the module is off; the reason form checks the module
    // itself and the template form checks the Admin level itself.
    { type: 'modal', prefix: 'ticket-modal', handler: handleTicketModal, module: null },
  ],
  jobs: [],
  provides: {},
};
