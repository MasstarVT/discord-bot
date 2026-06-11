# Games Suite — Design Spec
**Date:** 2026-06-11  
**Status:** Approved

---

## Context

Add a self-contained suite of 6 interactive mini-games and casino games to the existing discord.js v14 bot. All games use Slash Commands, Buttons, and where applicable Message Collectors, following the bot's existing file-system-as-registry pattern.

---

## File Organization

```
src/commands/games/
  rps.js           — Rock Paper Scissors
  tictactoe.js     — Tic-Tac-Toe (2-player)
  numguess.js      — Number Guessing Game
  blackjack.js     — Single-Deck Blackjack
  slots.js         — 3x3 Slot Machine
  roulette.js      — European Roulette
```

Each file exports `{ data, execute, permissionLevel }` per the existing pattern. Embeds use `infoEmbed`, `successEmbed`, `errorEmbed`, `warnEmbed` from `src/utils/embedBuilder.js` and `COLORS` from `src/config/constants.js`.

---

## Shared Conventions

### Active Game Guard
Each module exports a `const activeGames = new Set()` at module scope. `execute()` adds `interaction.user.id` on start and removes it on every exit path (win, loss, timeout, decline). If the user's ID is already in the set, reply with an error and return early.

### Collector ID Namespacing
Button custom IDs are scoped per invocation: `<game>:<interaction.id>:<action>` (e.g., `rps:123456:rock`). Collector filters also check `i.customId.startsWith(...)` to prevent cross-game interference in concurrent sessions.

### Security Filter
Collectors use `filter: () => true` (accept all clicks). Inside the `collect` handler, check if `i.user.id` is the authorized player. If not, reply ephemerally: `i.reply({ content: "This isn't your game!", ephemeral: true })`. This prevents the Discord "This interaction failed" popup for unauthorized users, which would appear if we silently dropped their interaction via a restrictive filter.

### Collector Timeout Handling
Every collector has an `on('end', collected => {...})` handler that:
1. Disables all buttons on the original message.
2. Edits the message to show a timeout notice if no result was reached.
3. Calls `activeGames.delete(interaction.user.id)`.

---

## Game Designs

### 1. Rock Paper Scissors (`rps.js`)

**Command:** `/rps`  
**Collector:** `createMessageComponentCollector({ time: 30_000 })`  
**Flow:**
1. Reply with embed "Choose your move!" + 3 buttons: 🪨 Rock, 📄 Paper, ✂️ Scissors.
2. On button click: if wrong user → ephemeral "This isn't your game!". If correct user → bot picks randomly, determines winner via lookup table, edits message with result embed (disabled buttons).
3. On timeout: edit message to "⏰ Time's up!" with all buttons disabled.

**Win determination:** `{ rock: 'scissors', paper: 'rock', scissors: 'paper' }` — player choice beats the value; bot choice beats the key.

---

### 2. Tic-Tac-Toe (`tictactoe.js`)

**Command:** `/tictactoe target:user`  
**Collector:** Two sequential `createMessageComponentCollector` instances.

**Phase 1 — Challenge (30s):**
- Reply with embed: "@target, you've been challenged! Do you accept?"
- Buttons: `✅ Accept` / `❌ Decline` (custom IDs: `ttt-challenge:<id>:accept|decline`)
- Filter: only `target.id` can accept/decline; others get ephemeral "This challenge isn't for you."
- On decline: edit to "Challenge declined." Cleanup.
- On timeout: edit to "Challenge timed out." Cleanup.

**Phase 2 — Game (60s per move):**
- State: `board = Array(9).fill('')`, `marks = { [challenger.id]: 'X', [target.id]: 'O' }`, `current = challenger`
- Render: 3 ActionRows × 3 Buttons. Empty cells labeled `⬜`, filled cells labeled `X`/`O` and disabled.
- Each move: mark cell, re-render board, check win/draw.
- Win conditions (indices): `[0,1,2],[3,4,5],[6,7,8],[0,3,6],[1,4,7],[2,5,8],[0,4,8],[2,4,6]`
- On win: disable all buttons, announce winner.
- On draw: disable all buttons, announce draw.
- On timeout: edit to "Game timed out (no moves made in time)." Disable all.

---

### 3. Number Guessing (`numguess.js`)

**Command:** `/numguess`  
**Collector:** `channel.createMessageCollector({ filter, time: 120_000 })`

**Flow:**
1. Pick `secret = Math.floor(Math.random() * 100) + 1`.
2. Reply with embed "I'm thinking of a number between 1 and 100. You have 7 guesses!"
3. Collector filter: `msg.author.id === interaction.user.id && /^\d+$/.test(msg.content.trim())`
4. On each message:
   - Increment `guessCount`.
   - Parse guess.
   - If `guess === secret`: reply "🎉 Correct! You got it in X/7 guesses.", `solved = true`, `collector.stop('won')`.
   - If `guessCount === 7`: `collector.stop('exhausted')`.
   - Else: reply `guess < secret ? '📈 Higher!' : '📉 Lower!'` with remaining guesses count.
5. `on('end', (_, reason)`:
   - If `reason !== 'won'`: reply "❌ Out of guesses! The number was **{secret}**."
   - `activeGames.delete(userId)`.

---

### 4. Blackjack (`blackjack.js`)

**Command:** `/blackjack bet:<int>`  
**Collector:** `createMessageComponentCollector({ time: 60_000 })`

**Deck:** 52 cards. Suits: `['♠️','♥️','♦️','♣️']`. Ranks: `A,2–10,J,Q,K`. Values: number cards face value; J/Q/K = 10; A = 11 (reduced to 1 if hand > 21). Fisher-Yates shuffle.

**Wallet:** `STARTING_BALANCE = 1000`. Validate `1 ≤ bet ≤ 1000`.

**Initial deal:** Player gets cards[0], cards[2]; dealer gets cards[1], cards[3].

**Render function `buildEmbed(state)`:**
- Player hand: all cards visible with suit emoji + rank.
- Dealer hand: during play = `[card1] [🂠 Hidden]`; on reveal = all cards.
- Shows hand totals. Shows bet and current balance.

**Natural blackjack check:** If player starts with 21 → immediate win (2.5× payout), skip to result.

**Hit:** Draw next card from deck, recalculate hand value, check bust. If bust → loss. If 21 → auto-stand.

**Stand / Dealer play:** Dealer reveals hidden card. Dealer draws while `dealerValue < 17` OR (`dealerValue === 17` AND hand is soft — contains an Ace counted as 11). Compare totals: player > dealer and player ≤ 21 → win (2×); equal → push; else → loss.

**Result embed:** Shows final hands, outcome, payout, and new balance.

---

### 5. Slots (`slots.js`)

**Command:** `/slots bet:<int>`  
**No interaction after initial roll.**

**Symbols (weighted):**
| Symbol | Weight | Payout (per line) |
|--------|--------|-------------------|
| 💎     | 1      | 5×                |
| 7️⃣    | 2      | 4×                |
| 🍇     | 3      | 3×                |
| 🍒     | 4      | 2.5×              |
| 🍋     | 5      | 2×                |
| 🍎     | 6      | 1.5×              |

**Roll:** Weighted random selection × 9 cells. Layout as `grid[row][col]`.

**Win check:** 5 lines — rows 0, 1, 2 and diagonals `[0,4,8]` and `[2,4,6]`. A line wins if all 3 symbols match.

**Payout:** Sum of all winning lines' multipliers × bet. If no wins → 0.

**Embed:** Renders grid with winning lines annotated with `◀ WIN!`. Shows total payout and new balance.

---

### 6. Roulette (`roulette.js`)

**Command:** `/roulette bet:<int> space:<string>`

**Wheel:** Numbers 0–36. 0 = Green. Red numbers: `1,3,5,7,9,12,14,16,18,19,21,23,25,27,30,32,34,36`. Black: remaining 1–36.

**Space validation:** Case-insensitive. Accepts `red`, `black`, `green`, `even`, `odd`, or integer string `0`–`36`. Any other value → ephemeral error embed.

**Spin animation (3 edits with `await sleep(1500)`):**
1. "🎰 Spinning... 🔴 ⚫ 🟢 🔴 ⚫ 🔴 ⚫"
2. "🎰 Slowing down... ⚫ 🔴 🟢 ⚫ 🔴"
3. "🎰 Almost there... 🔴 🟢 ⚫ 🔴 🟢"
4. Final reveal: "The ball lands on **{number} {color emoji}**!"

**Payout rules:**
| Space type       | Condition                           | Payout |
|------------------|-------------------------------------|--------|
| `red` / `black`  | Winning number matches color        | 2×     |
| `even` / `odd`   | Winning number ≠ 0 and parity matches | 2×   |
| `green`          | Winning number = 0                  | 18×    |
| Exact number     | Winning number = space              | 36×    |

0 always loses Red/Black/Even/Odd bets.

---

## Verification Plan

1. Run `npm run deploy` to register the 6 new slash commands globally (or guild-scoped via `DEPLOY_GUILD_ID`).
2. Test each game in a Discord server:
   - RPS: verify all 9 win/loss/draw combos and timeout behavior.
   - Tic-Tac-Toe: test accept, decline, timeout, win (X), win (O), draw; verify non-player cannot click.
   - Number Guess: verify 7-guess limit, correct guess ends early, timeout reveals number.
   - Blackjack: test bust, stand, push, natural blackjack, soft-17 dealer rule.
   - Slots: verify winning line detection and payout math.
   - Roulette: test all 5 space types, verify 0 loses Red/Black/Even/Odd, spin animation edits.
3. Run two concurrent `/rps` sessions as the same user to verify the active-game guard fires.
