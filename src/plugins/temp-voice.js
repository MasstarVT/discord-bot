// Temporary voice channels plugin declaration (pkg-temp-voice.md §5). Declaration only: every body lives in
// services/tempVoiceService.js and nothing runs at import.
import * as tv from '../services/tempVoiceService.js';

export const plugin = {
  id: 'temp-voice',
  module: 'tempvoice',
  hooks: [
    // All three hooks are `always`: emptied channels, deleted channels and reconciliation must keep draining while the module is off.
    // onVoiceStateUpdate checks the module itself before it creates a channel from a hub, and every hook body returns after queueing.
    { event: 'guildReady', id: 'temp-voice', always: true, run: tv.onGuildReady },
    { event: 'voiceStateUpdate', id: 'temp-voice', always: true, filter: tv.voiceFilter, run: tv.onVoiceStateUpdate },
    { event: 'channelDelete', id: 'temp-voice', always: true, filter: tv.channelDeleteFilter, run: tv.onChannelDelete },
  ],
  components: [
    // Gated by the plugin loader's module wrapper: while the module is off a panel click answers "The Temporary Channels module is
    // turned off on this server." These prefixes stay registered forever (persistent); a stale channel id answers "Channel Gone".
    { type: 'button', prefix: 'voice', handler: tv.handleButton },
    { type: 'select', prefix: 'voice-select', handler: tv.handleSelect },
    { type: 'modal', prefix: 'voice-modal', handler: tv.handleModal },
  ],
  jobs: [
    // Never checks the module toggle: keep-alive deletion must run while the module is off as well.
    { type: 'tempvoice.cleanup', handler: tv.handleCleanupJob },
  ],
  provides: {},
};
