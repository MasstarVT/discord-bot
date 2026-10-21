# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Primary: Discord server owners and admins (Manage Server) deciding whether to invite the hosted Frost.Byte bot to their server, then configuring it. They arrive from a link or a bot list, compare it against MEE6 and similar bots, and want to know quickly what it does, whether it's free, and how much setup it takes.

Secondary: developers who want to self-host it from the public GitHub repo (https://github.com/MasstarVT/discord-bot). They get a link, not the spotlight.

## Product Purpose

Frost.Byte is a multi-purpose Discord bot: 28 modules (moderation, automod, logs, welcome, levels, tickets, economy, games, feeds and more), each switched on or off per server with `/module` or the web dashboard. Success: a server owner invites it, enables the modules they need, and tunes them from the dashboard without reading a manual.

## Positioning

A MEE6-style feature set with no premium tier and no API key to buy: every built-in feature runs on Discord data or public, keyless endpoints. Some features go beyond MEE6: Auto-Translate, Counting, Auto Timestamps, a full two-player Scrabble, honeypot channels in automod, and a per-server economy with no premium limits.

## Operating Context

- Visitors evaluate in a browser, then act in Discord: "Add to Discord" (OAuth invite) and "Log in" to the dashboard at `/dashboard`.
- Inside Discord the bot is driven by 81 slash commands; `/help` shows only what a member can use. Admin commands need Manage Server.
- The dashboard is a separate process (`dashboard.js`) that serves the landing page at `/`, plus `/privacy` and `/terms`.

## Capabilities and Constraints

- Module list, categories, descriptions, two-letter codes and every module's settings fields (label, description, default) come from `src/dashboard/catalog.js`; the landing page must show every module's code, label and description and every category label (test-checked).
- Strict CSP: `script-src 'self'`, no inline scripts or styles, fonts only from fonts.gstatic.com unless the CSP changes. Every served path is listed in `server.js` and the landing test.
- No versions or tags exist; the changelog is dated, not versioned.
- Open: the support Discord server invite. It is configured per deployment (env var) and the page shows nothing until it is set.

## Brand Commitments

- Name: Frost.Byte (with the dot). Owner/author handle: MasstarVT.
- Winter / ice identity. The mark is a hexagonal ice crystal with a snowflake inside and a three-tooth bite taken out of its top-right edge ("frost-bite"); it's in `src/dashboard/public/favicon.svg` and the `#fb-mark` symbol on the public pages.
- Voice: plain, specific, no hype; say what a command does.

## Evidence on Hand

- Real: the module catalog, the command definitions in `src/commands/`, the bot's actual reply strings and embeds in the source, the rank/welcome card renderer (`src/utils/profileCard.js`), the git history (105 commits since 2026-06-11), the public GitHub repo and its Issues page.
- Absent, must not be invented: user counts, server counts, testimonials, uptime figures, ratings, version numbers, a support server link (until the owner supplies one).
- Any sample data in demos (member names, XP numbers) is labeled as sample data.

## Product Principles

1. Show the bot doing the job; never claim what the page can't show.
2. Every number and command on the page comes from the code.
3. Free means free: no premium upsell, no fake scarcity.
4. Admins should reach "it's running in my server" in minutes.
