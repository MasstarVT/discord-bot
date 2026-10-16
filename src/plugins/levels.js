// Levels plugin declaration (pkg-levels.md §6): message XP, voice XP and the role cleanup, the voice sweep job and the
// `xp` rewards provider. Declaration only: no logic and no side effects at import.
import {
  handleMessageXp, handleVoiceStateXp, handleRoleDelete, seedVoiceSessions, runVoiceSweep, voiceChangeMatters, xpRewardProvider,
} from '../services/xpService.js';

export const plugin = {
  id: 'levels',
  module: 'leveling',
  hooks: [
    // The core already dropped bots, webhooks, DMs and system messages; the filter only needs a resolved member.
    { event: 'messageCreate', id: 'levels', filter: (ctx) => !!ctx.member, run: handleMessageXp },
    // Mute, camera and stream toggles change nobody's voice XP: the sync filter skips them before any settings or Redis work.
    { event: 'voiceStateUpdate', id: 'levels', filter: voiceChangeMatters, run: handleVoiceStateXp },
    // Role cleanup runs even while the module is off, so a deleted role never lingers in rewards, no-XP or boost lists.
    { event: 'roleDelete', id: 'levels', always: true, run: handleRoleDelete },
    // Starts voice sessions for members who are already in a channel at startup or when the guild arrives.
    { event: 'guildReady', id: 'levels', run: seedVoiceSessions, timeoutMs: 30_000 },
  ],
  components: [],
  jobs: [{ type: 'levels.voiceSweep', handler: runVoiceSweep }],
  provides: { xp: xpRewardProvider },
};
