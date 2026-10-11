// /ticket (pkg-tickets): the per-ticket commands (MEE6 /ticket-claim, -close, -reopen, -delete, plus add, remove, transcript and rename).
// Thin: it defers, finds the ticket of the current channel and calls the service; staff checks and every rule live in
// services/ticketService.js. Every reply to the invoker is ephemeral; the public effects are notices the service posts itself.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  addMember, canPerform, claimTicket, closeTicket, deleteTicket, errorPayload, getTicketByChannel, isTicketStaff, removeMember,
  renameTicket, reopenTicket, transcriptForActor,
} from '../../services/ticketService.js';
import { infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { awaitConfirm, safeDefer, safeReply } from '../../utils/interactionHelpers.js';

export const module = 'tickets';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER; // openers must be able to close their own ticket; staff checks are in the service
// No setDefaultMemberPermissions and no allowInDMs: buildCommandPayload registers the command for servers only.

export const data = new SlashCommandBuilder()
  .setName('ticket')
  .setDescription('Manage the ticket in this channel (MEE6 /ticket-claim, -close, -reopen, -delete).')
  .addSubcommand((s) => s.setName('claim').setDescription('Claim this ticket so the team knows you handle it (MEE6 /ticket-claim).'))
  .addSubcommand((s) => s.setName('close').setDescription('Close this ticket; the opener can still read it (MEE6 /ticket-close).')
    .addStringOption((o) => o.setName('reason').setDescription('Why it is being closed (shown in the ticket)').setMaxLength(500)))
  .addSubcommand((s) => s.setName('reopen').setDescription('Reopen this closed ticket (MEE6 /ticket-reopen).'))
  .addSubcommand((s) => s.setName('delete').setDescription('Save the transcript and delete this ticket channel (MEE6 /ticket-delete).'))
  .addSubcommand((s) => s.setName('add').setDescription('Give a member access to this ticket.')
    .addUserOption((o) => o.setName('user').setDescription('Member to add').setRequired(true)))
  .addSubcommand((s) => s.setName('remove').setDescription('Remove a member you added from this ticket.')
    .addUserOption((o) => o.setName('user').setDescription('Member to remove').setRequired(true)))
  .addSubcommand((s) => s.setName('transcript').setDescription('Get a transcript file of this ticket (only you see it).'))
  .addSubcommand((s) => s.setName('rename').setDescription('Rename this ticket channel (Discord allows 2 renames per 10 minutes).')
    .addStringOption((o) => o.setName('name').setDescription('New channel name').setRequired(true).setMinLength(1).setMaxLength(90)));

const ACTIONS = { claim: claimTicket, close: closeTicket, reopen: reopenTicket, add: addMember, remove: removeMember, rename: renameTicket };
const SUCCESS_TEXT = {
  claim: 'Ticket claimed.', close: 'Ticket closed.', reopen: 'Ticket reopened.', add: 'Member added.', remove: 'Member removed.', rename: 'Channel renamed.',
};
const notesBlock = (notes) => (notes?.length ? `\n\n${notes.map((n) => `⚠️ ${n}`).join('\n')}` : '');

export async function execute(interaction, client) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  // ephemeral also on follow-ups: after the confirm dialog a reply is a follow-up, and those are public unless flagged.
  const reply = (payload) => safeReply(interaction, { ...payload, ephemeral: true });
  const { guild, guildId, options } = interaction;
  const sub = options.getSubcommand();

  const ticket = await getTicketByChannel(guildId, interaction.channelId);
  if (!ticket) return reply(errorPayload({ code: 'NOT_TICKET' }));
  if (ticket.status === 'DELETED') return reply(errorPayload({ code: 'TICKET_DELETED' }));

  const settings = peekSettings(guildId) ?? await getSettingsSafe(guildId);
  const ctx = { client, guild, ticket, actor: interaction.member, settings, source: 'command' };

  if (sub === 'transcript') {
    const r = await transcriptForActor({ guild, ticket, channel: interaction.channel, settings, actor: interaction.member });
    return reply(r.ok ? r.payload : errorPayload(r));
  }

  if (sub === 'delete') {
    const can = canPerform('delete', { isOwner: interaction.user.id === ticket.ownerId, isStaff: isTicketStaff(interaction.member, ticket.panel, settings) });
    if (!can.ok) return reply(errorPayload(can));
    if (ticket.status !== 'CLOSED') {
      const ok = await awaitConfirm(interaction, {
        title: 'Delete Ticket?',
        description: 'This ticket is still open. Its transcript will be saved (if transcripts are set up) and the channel deleted.',
        confirmLabel: 'Delete',
        ephemeral: true,
      });
      if (!ok) return reply({ embeds: [infoEmbed('Cancelled', 'The ticket was not deleted.')], components: [] });
    }
    const r = await deleteTicket({
      ...ctx,
      onSaved: async (info) => { await reply({ embeds: [successEmbed('Deleting Ticket', info.text)], components: [] }); },
    });
    // On success the channel that holds this interaction is gone: touching it would only produce 10003 errors.
    return r.ok ? undefined : reply({ ...errorPayload(r), components: [] });
  }

  const extra = {};
  if (sub === 'close') extra.reason = options.getString('reason')?.trim() || null;
  if (sub === 'add' || sub === 'remove') {
    extra.user = options.getUser('user', true);
    extra.member = options.getMember('user');
  }
  if (sub === 'rename') extra.name = options.getString('name', true);

  const result = await ACTIONS[sub]({ ...ctx, ...extra });
  if (!result.ok) return reply(errorPayload(result));
  return reply({ embeds: [successEmbed('Done', SUCCESS_TEXT[sub] + notesBlock(result.notes))], allowedMentions: { parse: [] } });
}
