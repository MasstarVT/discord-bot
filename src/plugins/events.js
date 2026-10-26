// Events plugin declaration: the RSVP and Add to calendar buttons and the edit form, the `event.remind` / `event.end` jobs and the hooks that
// keep events consistent when their post, channel, role or Discord event vanishes. Declaration only: the logic lives in
// services/eventService.js and nothing runs at import.
import { GuildScheduledEventStatus } from 'discord.js';
import {
  handleEditModal, handleEndJob, handleRemindJob, handleRsvpButton, onChannelDelete, onDiscordEventCancelled, onMessageDelete,
  onRoleDelete,
} from '../services/eventService.js';

// Event posts are bot-authored; a partial (uncached) message has no author and is looked up by id.
const isOwnOrUnknown = (ctx) => !ctx.message?.author || ctx.message.author.id === ctx.client?.user?.id;
const isCancelled = (ctx) => ctx.scheduledEvent?.status === GuildScheduledEventStatus.Canceled;

export const plugin = {
  id: 'events',
  module: 'events',
  hooks: [
    // `always`: cleanup must run even while the module is off, so a dead post or channel never keeps an event open.
    { event: 'messageDelete', id: 'events', always: true, filter: isOwnOrUnknown, run: (ctx) => onMessageDelete(ctx) },
    { event: 'channelDelete', id: 'events', always: true, timeoutMs: 30_000, run: (ctx) => onChannelDelete(ctx) },
    { event: 'roleDelete', id: 'events', always: true, run: (ctx) => onRoleDelete(ctx) },
    // Cancelling the Discord event in the server's Events tab deletes it (or sets it to Canceled): the event is cancelled too.
    {
      event: 'guildScheduledEventUpdate', id: 'events', always: true, timeoutMs: 30_000, filter: isCancelled,
      run: (ctx) => onDiscordEventCancelled(ctx),
    },
    { event: 'guildScheduledEventDelete', id: 'events', always: true, timeoutMs: 30_000, run: (ctx) => onDiscordEventCancelled(ctx) },
  ],
  components: [
    // Module gating is applied by installPlugins (plugin.module = 'events').
    { type: 'button', prefix: 'event', handler: (interaction, client) => handleRsvpButton(interaction, client) },
    { type: 'modal', prefix: 'event-modal', handler: (interaction, client) => handleEditModal(interaction, client) },
  ],
  jobs: [
    { type: 'event.remind', handler: (job, ctx) => handleRemindJob(job, ctx) },
    { type: 'event.end', handler: (job, ctx) => handleEndJob(job, ctx) },
  ],
  provides: {},
};
