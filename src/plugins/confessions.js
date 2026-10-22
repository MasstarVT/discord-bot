// Confessions plugin declaration: the cleanup hooks (a deleted post, review message or channel), the persistent buttons (the
// panel's Confess button and the review message's Approve / Reject) and the confession form. Declaration only: the rules live in
// services/confessionService.js and nothing runs at import.
import { ChannelType } from 'discord.js';
import {
  handleButton, handleConfessionModal, onChannelDelete, onMessageDelete,
} from '../services/confessionService.js';

// Posts and review messages are bot-authored; a partial (uncached) message has no author and is looked up by id.
const isOwnOrUnknown = (ctx) => ctx.message?.partial === true || ctx.message?.author?.id === ctx.client?.user?.id;

// The confession and review channels are text or announcement channels; other deletions never reach the database.
const RELEVANT = new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement]);

export const plugin = {
  id: 'confessions',
  module: 'confessions',
  hooks: [
    // `always`: forgetting a dead message id or an unset channel is cleanup and must happen while the module is off as well.
    { event: 'messageDelete', id: 'confessions', always: true, filter: isOwnOrUnknown, run: (ctx) => onMessageDelete(ctx) },
    { event: 'channelDelete', id: 'confessions', always: true, filter: (ctx) => RELEVANT.has(ctx.channel?.type), run: (ctx) => onChannelDelete(ctx) },
  ],
  components: [
    // Gated by the foundation wrapper (plugin.module = 'confessions'): while the module is off the buttons and the form answer
    // "The Confessions module is turned off on this server." and nothing is recorded.
    { type: 'button', prefix: 'confession', handler: (interaction, client) => handleButton(interaction, client) },
    { type: 'modal', prefix: 'confession-modal', handler: (interaction, client) => handleConfessionModal(interaction, client) },
  ],
  jobs: [],
  provides: {},
};
