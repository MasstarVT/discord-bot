# Changelog

What changed in Frost.Byte, newest first. Each entry is dated by when it landed on `main`; there are no version numbers yet. The full history is in the [commit log](https://github.com/MasstarVT/discord-bot/commits/main).

## 2026-10-27 · Maths in Counting
- Counting accepts maths: `2+2`, `(3*4)-1` or `2^3` counts as the number it comes to. Sums that come to a fraction, such as `10/27`, are treated as chat and never reset the count.

## 2026-10-21 · Go-live posts for the Live Role
- The Live Role can now post in a channel when a member goes live, with a link to their Twitch or YouTube stream. Set it up with `/liverole announce channel:#live-now`, or on the dashboard; the message can be customised with `{url}`, `{platform}`, `{title}` and `{game}`, and `ping_role` pings a role such as @Stream Alerts with each post.

## 2026-10-20 · Auto Timestamps and a new site
- A new public site: the landing page opens every module to show its commands and default settings, runs sample commands with the bot's real replies, and lets you try the Levels settings. New `/docs` and `/changelog` pages list every module, command and change.
- New module **Auto Timestamps**: when a member types a time such as "friday 5pm" or "7pm EST", the bot replies with a Discord timestamp everyone reads in their own timezone. `/autotime enable` turns it on.
- Members with no timezone set are told how to pick a timezone role or use `/timezone set`.
- The dashboard's Audit Logs page lists every event that gets logged.

## 2026-10-19 · Public landing page and legal pages
- The dashboard got a new look, and the bot got a public landing page.
- Added a privacy policy and terms of service, plus trusted-proxy settings so the dashboard can be served publicly.
- Dependency security updates (js-yaml, brace-expansion, ip-address).

## 2026-10-18 · Honeypot channels
- Automod can now watch **honeypot channels**: whoever posts in one is softbanned (or banned, kicked or timed out) at once, which catches spam bots and hacked accounts. `/automod honeypot add` can create the channel and post a warning in it.

## 2026-10-17 · More reaction-role templates
- Reaction roles gained ready-made role sets: pronouns, colours, age ranges and more.

## 2026-10-16 · Timezones
- Added `/timestamp`, `/timezone` and timezone roles, so times show up in each member's own timezone.
- Timezone reaction-role templates in `/reactionrole` and the dashboard, and a timezone-roles editor on the dashboard's General page.
- Custom commands limited to certain roles are now hidden from members who can't run them.

## 2026-10-15 · Performance and fixes
- A performance pass across the bot, including buffered achievement message counts.
- Added `/owner`, which pings the bot owner.
- Fixed the bugs found in a full codebase review.

## 2026-10-14 · Counting
- New module **Counting**: members count up from 1 in turns; a wrong number or a double turn starts it over.
- The bot owner can see who added the bot to each server.

## 2026-10-13 · Dashboard access
- The dashboard shows only the servers you manage; the bot owner can manage, and join, any server the bot is in.
- Translation activity per channel on the dashboard.
- Setup commands can create the role or channel they ask for.
- Moderator commands now require default member permissions too.

## 2026-10-11 · The web dashboard
- Added the web dashboard: log in with Discord and change every module's settings in a browser.
- Manage saved embeds, custom commands, reminders, feeds, level rewards, the shop and achievements from it.
- Run giveaways and polls, build automations, manage reaction-role and ticket panels, and moderate members (XP and coins too) from the dashboard.
- An embed editor, member pickers and long-list paging.
- The bot itself was rebuilt for feature parity with MEE6, with the audited bugs fixed.

## 2026-09-27 · Docker and CI
- Docker image, health endpoints, CI with GitHub Container Registry publishing, rollback and Dependabot.
- A LibreTranslate backend for translation, with back-off on rate limits.
- A failing event handler no longer restarts the bot; a shard that disconnects for good stops it cleanly.

## 2026-08-23 · README and license
- Added the README and the MIT license.

## 2026-06-11 · First version
- The first version of the bot, including the games command and Scrabble.
