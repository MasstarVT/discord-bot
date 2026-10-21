// Suggestions plugin declaration: the cleanup hooks (a deleted post or suggestion channel), the persistent vote buttons and
// the submit form. Declaration only: the rules live in services/suggestionService.js and nothing runs at import.
import { ChannelType } from 'discord.js';
import {
  handleSuggestionModal, handleVoteButton, onChannelDelete, onMessageDelete,
} from '../services/suggestionService.js';

// Suggestion posts are bot-authored; a partial (uncached) message has no author and is looked up by id.
const isOwnOrUnknown = (ctx) => ctx.message?.partial === true || ctx.message?.author?.id === ctx.client?.user?.id;

// The suggestion channel (and so every post) is a text or announcement channel; other deletions never reach the database.
const RELEVANT = new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement]);

export const plugin = {
  id: 'suggestions',
  module: 'suggestions',
  hooks: [
    // `always`: clearing a dead message id or an unset channel is cleanup and must happen while the module is off as well.
    { event: 'messageDelete', id: 'suggestions', always: true, filter: isOwnOrUnknown, run: (ctx) => onMessageDelete(ctx) },
    { event: 'channelDelete', id: 'suggestions', always: true, filter: (ctx) => RELEVANT.has(ctx.channel?.type), run: (ctx) => onChannelDelete(ctx) },
  ],
  components: [
    // Gated by the foundation wrapper (plugin.module = 'suggestions'): while the module is off the buttons and the form answer
    // "The Suggestions module is turned off on this server."
    { type: 'button', prefix: 'suggestion', handler: (interaction, client) => handleVoteButton(interaction, client) },
    { type: 'modal', prefix: 'suggestion-modal', handler: (interaction, client) => handleSuggestionModal(interaction, client) },
  ],
  jobs: [],
  provides: {},
};
