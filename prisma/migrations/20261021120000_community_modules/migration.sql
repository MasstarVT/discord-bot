-- CreateEnum
CREATE TYPE "SuggestionStatus" AS ENUM ('OPEN', 'CONSIDERED', 'APPROVED', 'DENIED', 'IMPLEMENTED');

-- CreateEnum
CREATE TYPE "ModmailStatus" AS ENUM ('OPEN', 'CLOSED');

-- CreateEnum
CREATE TYPE "ApplicationStatus" AS ENUM ('DRAFT', 'PENDING', 'ACCEPTED', 'DENIED');

-- CreateEnum
CREATE TYPE "AntiRaidAction" AS ENUM ('ALERT', 'KICK', 'BAN');

-- CreateEnum
CREATE TYPE "GuildEventStatus" AS ENUM ('SCHEDULED', 'ACTIVE', 'ENDED', 'CANCELLED');

-- CreateEnum
CREATE TYPE "RsvpStatus" AS ENUM ('GOING', 'MAYBE', 'DECLINED');

-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "afkEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "afkIgnoredChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "afkNicknamePrefix" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "antiRaidAction" "AntiRaidAction" NOT NULL DEFAULT 'ALERT',
ADD COLUMN     "antiRaidAlertChannelId" TEXT,
ADD COLUMN     "antiRaidEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "antiRaidJoinSeconds" INTEGER NOT NULL DEFAULT 10,
ADD COLUMN     "antiRaidJoinThreshold" INTEGER NOT NULL DEFAULT 10,
ADD COLUMN     "antiRaidLockChat" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "antiRaidLockdownMinutes" INTEGER NOT NULL DEFAULT 30,
ADD COLUMN     "antiRaidMinAccountAgeDays" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "antiRaidPauseInvites" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "antiRaidYoungAction" "AntiRaidAction" NOT NULL DEFAULT 'KICK',
ADD COLUMN     "applicationsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "eventChannelId" TEXT,
ADD COLUMN     "eventCreateDiscordEvent" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "eventReminderMinutes" INTEGER NOT NULL DEFAULT 15,
ADD COLUMN     "eventsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "modmailAnonymousReplies" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "modmailBlockedUserIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "modmailChannelId" TEXT,
ADD COLUMN     "modmailEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "modmailGreeting" TEXT,
ADD COLUMN     "modmailPingRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "rolePersistDays" INTEGER NOT NULL DEFAULT 90,
ADD COLUMN     "rolePersistEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "rolePersistIgnoredRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "rolePersistNicknames" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "stickyEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "suggestionAllowAnonymous" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "suggestionChannelId" TEXT,
ADD COLUMN     "suggestionCooldownSeconds" INTEGER NOT NULL DEFAULT 300,
ADD COLUMN     "suggestionDmOnUpdate" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "suggestionThreads" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "suggestionsEnabled" BOOLEAN NOT NULL DEFAULT false;

-- CreateTable
CREATE TABLE "Suggestion" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "number" INTEGER NOT NULL,
    "authorId" TEXT NOT NULL,
    "anonymous" BOOLEAN NOT NULL DEFAULT false,
    "content" TEXT NOT NULL,
    "status" "SuggestionStatus" NOT NULL DEFAULT 'OPEN',
    "channelId" TEXT NOT NULL,
    "messageId" TEXT,
    "threadId" TEXT,
    "upvotes" INTEGER NOT NULL DEFAULT 0,
    "downvotes" INTEGER NOT NULL DEFAULT 0,
    "reviewerId" TEXT,
    "reviewNote" TEXT,
    "reviewedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Suggestion_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "SuggestionVote" (
    "suggestionId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "value" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "SuggestionVote_pkey" PRIMARY KEY ("suggestionId","userId")
);

-- CreateTable
CREATE TABLE "ModmailThread" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "threadId" TEXT NOT NULL,
    "status" "ModmailStatus" NOT NULL DEFAULT 'OPEN',
    "closedById" TEXT,
    "closedAt" TIMESTAMP(3),
    "closeReason" TEXT,
    "lastActivityAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "ModmailThread_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "ApplicationForm" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "description" TEXT,
    "questions" JSONB NOT NULL DEFAULT '[]',
    "reviewChannelId" TEXT,
    "acceptRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "requiredRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "cooldownHours" INTEGER NOT NULL DEFAULT 24,
    "acceptMessage" TEXT,
    "denyMessage" TEXT,
    "panelChannelId" TEXT,
    "panelMessageId" TEXT,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "ApplicationForm_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "ApplicationSubmission" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "formId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "answers" JSONB NOT NULL DEFAULT '[]',
    "status" "ApplicationStatus" NOT NULL DEFAULT 'DRAFT',
    "reviewerId" TEXT,
    "reviewNote" TEXT,
    "reviewMessageId" TEXT,
    "submittedAt" TIMESTAMP(3),
    "reviewedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "ApplicationSubmission_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "StickyMessage" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "content" TEXT,
    "embed" JSONB,
    "lastMessageId" TEXT,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdById" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "StickyMessage_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "AfkStatus" (
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "reason" TEXT,
    "nickChanged" BOOLEAN NOT NULL DEFAULT false,
    "previousNick" TEXT,
    "pings" JSONB NOT NULL DEFAULT '[]',
    "pingCount" INTEGER NOT NULL DEFAULT 0,
    "since" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "AfkStatus_pkey" PRIMARY KEY ("guildId","userId")
);

-- CreateTable
CREATE TABLE "AntiRaidState" (
    "guildId" TEXT NOT NULL,
    "lockedAt" TIMESTAMP(3),
    "lockedUntil" TIMESTAMP(3),
    "lockedById" TEXT,
    "reason" TEXT,
    "priorEveryoneSend" TEXT,
    "invitesPausedUntil" TIMESTAMP(3),
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "AntiRaidState_pkey" PRIMARY KEY ("guildId")
);

-- CreateTable
CREATE TABLE "GuildEvent" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "title" TEXT NOT NULL,
    "description" TEXT,
    "location" TEXT,
    "startsAt" TIMESTAMP(3) NOT NULL,
    "endsAt" TIMESTAMP(3),
    "hostId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "messageId" TEXT,
    "roleId" TEXT,
    "capacity" INTEGER,
    "reminderMinutes" INTEGER NOT NULL DEFAULT 15,
    "reminderSentAt" TIMESTAMP(3),
    "discordEventId" TEXT,
    "status" "GuildEventStatus" NOT NULL DEFAULT 'SCHEDULED',
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "GuildEvent_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "EventRsvp" (
    "eventId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "status" "RsvpStatus" NOT NULL,
    "roleGranted" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "EventRsvp_pkey" PRIMARY KEY ("eventId","userId")
);

-- CreateTable
CREATE TABLE "PersistedMember" (
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "roleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "nickname" TEXT,
    "leftAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "PersistedMember_pkey" PRIMARY KEY ("guildId","userId")
);

-- CreateIndex
CREATE UNIQUE INDEX "Suggestion_messageId_key" ON "Suggestion"("messageId");

-- CreateIndex
CREATE INDEX "Suggestion_guildId_status_idx" ON "Suggestion"("guildId", "status");

-- CreateIndex
CREATE INDEX "Suggestion_guildId_authorId_createdAt_idx" ON "Suggestion"("guildId", "authorId", "createdAt");

-- CreateIndex
CREATE UNIQUE INDEX "Suggestion_guildId_number_key" ON "Suggestion"("guildId", "number");

-- CreateIndex
CREATE UNIQUE INDEX "ModmailThread_threadId_key" ON "ModmailThread"("threadId");

-- CreateIndex
CREATE INDEX "ModmailThread_guildId_userId_status_idx" ON "ModmailThread"("guildId", "userId", "status");

-- CreateIndex
CREATE INDEX "ModmailThread_userId_status_idx" ON "ModmailThread"("userId", "status");

-- CreateIndex
CREATE UNIQUE INDEX "ApplicationForm_panelMessageId_key" ON "ApplicationForm"("panelMessageId");

-- CreateIndex
CREATE UNIQUE INDEX "ApplicationForm_guildId_name_key" ON "ApplicationForm"("guildId", "name");

-- CreateIndex
CREATE UNIQUE INDEX "ApplicationSubmission_reviewMessageId_key" ON "ApplicationSubmission"("reviewMessageId");

-- CreateIndex
CREATE INDEX "ApplicationSubmission_formId_userId_status_idx" ON "ApplicationSubmission"("formId", "userId", "status");

-- CreateIndex
CREATE INDEX "ApplicationSubmission_guildId_status_idx" ON "ApplicationSubmission"("guildId", "status");

-- CreateIndex
CREATE UNIQUE INDEX "StickyMessage_channelId_key" ON "StickyMessage"("channelId");

-- CreateIndex
CREATE INDEX "StickyMessage_guildId_idx" ON "StickyMessage"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "GuildEvent_messageId_key" ON "GuildEvent"("messageId");

-- CreateIndex
CREATE INDEX "GuildEvent_guildId_status_startsAt_idx" ON "GuildEvent"("guildId", "status", "startsAt");

-- CreateIndex
CREATE INDEX "EventRsvp_eventId_status_idx" ON "EventRsvp"("eventId", "status");

-- CreateIndex
CREATE INDEX "PersistedMember_guildId_leftAt_idx" ON "PersistedMember"("guildId", "leftAt");

-- AddForeignKey
ALTER TABLE "Suggestion" ADD CONSTRAINT "Suggestion_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "SuggestionVote" ADD CONSTRAINT "SuggestionVote_suggestionId_fkey" FOREIGN KEY ("suggestionId") REFERENCES "Suggestion"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ModmailThread" ADD CONSTRAINT "ModmailThread_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ApplicationForm" ADD CONSTRAINT "ApplicationForm_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ApplicationSubmission" ADD CONSTRAINT "ApplicationSubmission_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ApplicationSubmission" ADD CONSTRAINT "ApplicationSubmission_formId_fkey" FOREIGN KEY ("formId") REFERENCES "ApplicationForm"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "StickyMessage" ADD CONSTRAINT "StickyMessage_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "AfkStatus" ADD CONSTRAINT "AfkStatus_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "AntiRaidState" ADD CONSTRAINT "AntiRaidState_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "GuildEvent" ADD CONSTRAINT "GuildEvent_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "EventRsvp" ADD CONSTRAINT "EventRsvp_eventId_fkey" FOREIGN KEY ("eventId") REFERENCES "GuildEvent"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "PersistedMember" ADD CONSTRAINT "PersistedMember_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

