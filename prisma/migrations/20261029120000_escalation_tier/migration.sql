-- AlterTable
ALTER TABLE "Infraction" ADD COLUMN     "escalationTierId" INTEGER;

-- Backfill: escalation cases written before this column existed carried the tier only in their reason text
-- ("[Escalation T3] Reached 5 active warnings within 1 week"), so a tier that already fired must stay fired after deploy.
-- SQL cannot know the bot's user id, so the cases are recognised by what runEscalation writes: a tier-action type
-- (a temp ban is stored as BAN, so a WARN, UNMUTE or UNBAN row never matches) and the full generated sentence.
-- The tier id is capped at 9 digits so a typed "[Escalation T99999999999999]" cannot overflow the cast and fail the migration.
UPDATE "Infraction"
SET "escalationTierId" = substring("reason" from '^\[Escalation T([0-9]{1,9})\]')::integer
WHERE "escalationTierId" IS NULL
  AND "type" IN ('TIMEOUT', 'MUTE', 'KICK', 'BAN')
  AND "reason" ~ '^\[Escalation T[0-9]{1,9}\] Reached [0-9]+ active warning';
