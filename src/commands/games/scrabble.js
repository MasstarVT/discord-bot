import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, ComponentType, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId, parseCustomId } from '../../config/customIds.js';
import {
  CHALLENGE_TIMEOUT_MS, createGame, dictionaryStatus, finishGame, getDictionary, isChannelBusy,
  loadDictionary, postBoard, registerGame, releaseChannel, reserveChannel,
} from '../../services/scrabbleService.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { guildEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { clampText } from '../../utils/text.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('scrabble')
  .setDescription('Challenge another player to a game of Scrabble!')
  .addUserOption((o) =>
    o.setName('target').setDescription('The player you want to challenge').setRequired(true)
  );

// How long an accepted challenge may wait for the dictionary to finish loading (it started at challenge time).
const DICTIONARY_WAIT_MS = 5000;
const BUSY_TEXT = '⚠️ There is already a Scrabble game or a pending challenge in this channel!';

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'scrabble settings').catch(() => null);

const shownName = (member, user) => escapeMarkdown(clampText(member?.displayName ?? user.username, 32)) || 'Player';

export async function execute(interaction) {
  const challenger = interaction.user;
  const target = interaction.options.getUser('target');
  const channelId = interaction.channelId;
  const refuse = (content) => interaction.reply({ content, ephemeral: true });

  if (target.id === challenger.id) return refuse('❌ You cannot challenge yourself!');
  if (target.bot) return refuse('❌ You cannot challenge a bot!');
  const targetMember = interaction.options.getMember('target');
  if (!targetMember) return refuse("❌ That user isn't in this server.");
  if (isChannelBusy(channelId)) return refuse(BUSY_TEXT);

  void loadDictionary(); // starts the lazy load; memoised, never rejects, deliberately not awaited
  const settings = await loadSettings(interaction);
  const challengerName = shownName(interaction.member, challenger);
  const targetName = shownName(targetMember, target);

  // R085: the reservation is taken synchronously with the busy check, so two challenges can't coexist.
  if (!reserveChannel(channelId, interaction.id)) return refuse(BUSY_TEXT);

  // R089: the reservation belongs to the collector once it exists; release it if anything throws before.
  let handedOff = false;
  try {
    const row = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId(buildCustomId('scrabble-ch', interaction.id, 'accept')).setLabel('✅ Accept').setStyle(ButtonStyle.Success),
      new ButtonBuilder().setCustomId(buildCustomId('scrabble-ch', interaction.id, 'decline')).setLabel('❌ Decline').setStyle(ButtonStyle.Danger),
    );
    const response = await interaction.reply({
      content: `<@${target.id}>`,
      allowedMentions: { users: [target.id] },
      embeds: [guildEmbed(settings, '🔤 Scrabble Challenge', `**${challengerName}** challenges you to Scrabble! You have **30 seconds** to respond.`)],
      components: [row],
    });

    // The filter checks only this challenge's prefix; users are checked inside `collect`, so a wrong
    // user's click gets a friendly reply instead of the router's "expired" fallback.
    const collector = response.createMessageComponentCollector({
      componentType: ComponentType.Button,
      filter: (i) => i.customId.startsWith(`scrabble-ch:${interaction.id}:`),
      time: CHALLENGE_TIMEOUT_MS,
    });
    let settled = false;

    collector.on('collect', async (i) => {
      try {
        const action = parseCustomId(i.customId).params[1];

        if (settled) return void i.deferUpdate().catch(() => {});

        if (i.user.id === challenger.id) {
          if (action === 'decline') {
            // The challenger "declining" cancels their own challenge.
            settled = true;
            collector.stop('cancel');
            releaseChannel(channelId, interaction.id);
            await i.update({
              content: '',
              embeds: [guildEmbed(settings, '🔤 Scrabble', `**${challengerName}** cancelled the challenge.`)],
              components: [],
            });
            return;
          }
          await i.reply({ content: `⏳ Waiting for **${targetName}** to accept.`, ephemeral: true });
          return;
        }

        if (i.user.id !== target.id) {
          await i.reply({ content: "❌ This challenge isn't for you!", ephemeral: true });
          return;
        }

        if (action === 'decline') {
          settled = true;
          collector.stop('decline');
          releaseChannel(channelId, interaction.id);
          await i.update({
            content: '',
            embeds: [guildEmbed(settings, '🔤 Scrabble', `${targetName} declined the Scrabble challenge.`)],
            components: [],
          });
          return;
        }

        if (action !== 'accept') return void i.deferUpdate().catch(() => {});

        // Accept (R021, R086, R087): decide synchronously, so a second Accept can't start a second game.
        settled = true;
        collector.stop('accept');

        // Acknowledge first (00-overview rule 21). The channel stays reserved meanwhile, and `settled`
        // (plus the ended collector) blocks repeat clicks, so nothing can start a second game.
        await i.deferUpdate();

        // The dictionary load began when the challenge was issued, so this is normally instant.
        const dictionary = dictionaryStatus().state === 'loading'
          ? await withTimeout(loadDictionary(), DICTIONARY_WAIT_MS, 'scrabble dictionary').catch(() => null)
          : getDictionary();

        const game = createGame({
          id: interaction.id,
          guildId: interaction.guildId,
          channelId,
          settings,
          dictionary,
          players: [{ id: challenger.id, name: challengerName }, { id: target.id, name: targetName }],
        });
        if (!registerGame(game)) {
          releaseChannel(channelId, interaction.id);
          await i.followUp({ content: '⚠️ A Scrabble game already started in this channel.', ephemeral: true });
          return;
        }
        game.lastInteraction = i;

        let started = false;
        try {
          await i.editReply({
            content: '',
            embeds: [guildEmbed(settings, '🔤 Scrabble', `**${targetName}** accepted **${challengerName}**'s challenge. Good luck!`)],
            components: [],
          });
          started = await postBoard(game, i, {
            content: `<@${challenger.id}>, you go first! Play with \`/scrabble-play word\` — you have 3 minutes per turn.`,
            mentionIds: [challenger.id],
          });
        } catch (err) {
          if (isInteractionGone(err)) logger.debug('scrabble: accept interaction gone', err);
          else logger.error('Scrabble start failed', err);
        }
        if (!started) {
          // A game that never got its board would lock the channel until its timer ran out.
          finishGame(game, 'aborted');
          i.followUp({ content: "⚠️ I couldn't start the game. Please try `/scrabble` again.", ephemeral: true }).catch(() => {});
        }
      } catch (err) {
        if (isInteractionGone(err)) logger.debug('scrabble: interaction gone', err);
        else logger.error('Scrabble challenge collect failed', err);
        releaseChannel(channelId, interaction.id);
        if (i && !i.replied && !i.deferred) await i.deferUpdate().catch(() => {});
      }
    });

    collector.on('end', async (_collected, reason) => {
      try {
        if (reason === 'accept' || reason === 'decline' || reason === 'cancel') return;
        releaseChannel(channelId, interaction.id);
        if (reason === 'time') {
          await interaction.editReply({
            content: '',
            embeds: [warnEmbed('Challenge Timed Out', `${targetName} did not respond in time.`)],
            components: [],
          }).catch(() => {});
        }
      } catch (err) {
        logger.error('Scrabble challenge end failed', err);
      }
    });

    handedOff = true;
  } finally {
    if (!handedOff) releaseChannel(channelId, interaction.id);
  }
}
