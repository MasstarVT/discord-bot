// Reference data for the feat-moderation audit tests, produced by the JS code itself.
// `deletable`: discord.js's own `Message.prototype.deletable` getter for every message type 0..60, on a guild
// message in a channel where the bot can view and has Manage Messages (not Administrator) and is not timed out:
// what /purge's collectPurgeTargets sees. Prints `RESULT <json>`.
import { Message, PermissionFlagsBits, PermissionsBitField } from 'discord.js';

const getter = Object.getOwnPropertyDescriptor(Message.prototype, 'deletable').get;
const perms = new PermissionsBitField([PermissionFlagsBits.ViewChannel, PermissionFlagsBits.ManageMessages]);
const me = { isCommunicationDisabled: () => false };
const out = {};
for (let type = 0; type <= 60; type++) {
  const fake = {
    type,
    author: { id: '600000000000000001' },
    client: { user: { id: '700000000000000001' } },
    guild: { members: { me } },
    channel: { viewable: true, permissionsFor: () => perms },
  };
  out[type] = getter.call(fake);
}
console.log(`RESULT ${JSON.stringify({ deletable: out })}`);
