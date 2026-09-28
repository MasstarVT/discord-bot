-- CreateEnum
CREATE TYPE "InfractionType" AS ENUM ('WARN', 'MUTE', 'KICK', 'SOFTBAN', 'BAN', 'UNBAN', 'UNMUTE', 'TIMEOUT', 'NOTE');

-- CreateEnum
CREATE TYPE "ReactionRoleType" AS ENUM ('NORMAL', 'UNIQUE', 'VERIFY', 'REVERSE');

-- CreateEnum
CREATE TYPE "RRLayout" AS ENUM ('BUTTONS', 'SELECT');

-- CreateEnum
CREATE TYPE "TriggerMatchType" AS ENUM ('CONTAINS', 'EXACT', 'STARTS_WITH', 'ENDS_WITH', 'REGEX');

-- CreateTable
CREATE TABLE "GuildSettings" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    "modEnabled" BOOLEAN NOT NULL DEFAULT false,
    "loggingEnabled" BOOLEAN NOT NULL DEFAULT false,
    "welcomeEnabled" BOOLEAN NOT NULL DEFAULT false,
    "reactionRolesEnabled" BOOLEAN NOT NULL DEFAULT false,
    "levelingEnabled" BOOLEAN NOT NULL DEFAULT false,
    "customCommandsEnabled" BOOLEAN NOT NULL DEFAULT false,
    "automodEnabled" BOOLEAN NOT NULL DEFAULT false,
    "translationEnabled" BOOLEAN NOT NULL DEFAULT false,
    "modLogChannelId" TEXT,
    "modRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "muteRoleId" TEXT,
    "automodConfig" JSONB,
    "infractionEscalation" JSONB,
    "messageLogChannelId" TEXT,
    "memberLogChannelId" TEXT,
    "serverLogChannelId" TEXT,
    "voiceLogChannelId" TEXT,
    "joinLeaveChannelId" TEXT,
    "welcomeChannelId" TEXT,
    "welcomeEmbed" JSONB,
    "welcomeMessage" TEXT,
    "autoroleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "autoroleDelay" INTEGER NOT NULL DEFAULT 0,
    "xpEnabled" BOOLEAN NOT NULL DEFAULT false,
    "xpRate" INTEGER NOT NULL DEFAULT 15,
    "xpCooldown" INTEGER NOT NULL DEFAULT 60,
    "xpMultiplier" DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    "levelUpChannelId" TEXT,
    "levelUpMessage" TEXT,
    "noXpRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "noXpChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "levelRolesStack" BOOLEAN NOT NULL DEFAULT true,
    "voiceXpEnabled" BOOLEAN NOT NULL DEFAULT false,
    "voiceXpRate" INTEGER NOT NULL DEFAULT 5,

    CONSTRAINT "GuildSettings_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Infraction" (
    "id" SERIAL NOT NULL,
    "caseId" INTEGER NOT NULL,
    "guildId" TEXT NOT NULL,
    "targetUserId" TEXT NOT NULL,
    "moderatorId" TEXT NOT NULL,
    "type" "InfractionType" NOT NULL,
    "reason" TEXT,
    "proofUrl" TEXT,
    "active" BOOLEAN NOT NULL DEFAULT true,
    "expiresAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Infraction_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "UserLevel" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "xp" INTEGER NOT NULL DEFAULT 0,
    "level" INTEGER NOT NULL DEFAULT 0,
    "messageCount" INTEGER NOT NULL DEFAULT 0,
    "voiceMinutes" INTEGER NOT NULL DEFAULT 0,
    "lastXpAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "UserLevel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "LevelRole" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "level" INTEGER NOT NULL,
    "roleId" TEXT NOT NULL,

    CONSTRAINT "LevelRole_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "ReactionRoleGroup" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "messageId" TEXT NOT NULL,
    "title" TEXT NOT NULL,
    "description" TEXT,
    "maxRoles" INTEGER,
    "type" "ReactionRoleType" NOT NULL DEFAULT 'NORMAL',
    "layout" "RRLayout" NOT NULL DEFAULT 'BUTTONS',
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "ReactionRoleGroup_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "ReactionRoleItem" (
    "id" TEXT NOT NULL,
    "groupId" TEXT NOT NULL,
    "customId" TEXT NOT NULL,
    "roleId" TEXT NOT NULL,
    "label" TEXT NOT NULL,
    "emoji" TEXT,
    "style" INTEGER NOT NULL DEFAULT 1,
    "position" INTEGER NOT NULL DEFAULT 0,

    CONSTRAINT "ReactionRoleItem_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "CustomTag" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "content" TEXT NOT NULL,
    "createdBy" TEXT NOT NULL,
    "uses" INTEGER NOT NULL DEFAULT 0,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    "useEmbed" BOOLEAN NOT NULL DEFAULT false,

    CONSTRAINT "CustomTag_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "AutoTrigger" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "trigger" TEXT NOT NULL,
    "response" TEXT NOT NULL,
    "matchType" "TriggerMatchType" NOT NULL DEFAULT 'CONTAINS',
    "caseSensitive" BOOLEAN NOT NULL DEFAULT false,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "cooldown" INTEGER NOT NULL DEFAULT 0,
    "deleteMessage" BOOLEAN NOT NULL DEFAULT false,
    "channelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "exemptRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "uses" INTEGER NOT NULL DEFAULT 0,
    "createdBy" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "AutoTrigger_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "TranslateChannel" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "targetLangs" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "TranslateChannel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "GuildMemberCache" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "joinedAt" TIMESTAMP(3) NOT NULL,
    "inviterId" TEXT,
    "inviteCode" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "GuildMemberCache_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "UserEconomy" (
    "id" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "balance" INTEGER NOT NULL DEFAULT 1000,
    "lastDaily" TIMESTAMP(3),
    "lastPity" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "UserEconomy_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "GuildSettings_guildId_key" ON "GuildSettings"("guildId");

-- CreateIndex
CREATE INDEX "Infraction_guildId_targetUserId_idx" ON "Infraction"("guildId", "targetUserId");

-- CreateIndex
CREATE INDEX "Infraction_guildId_active_expiresAt_idx" ON "Infraction"("guildId", "active", "expiresAt");

-- CreateIndex
CREATE UNIQUE INDEX "Infraction_guildId_caseId_key" ON "Infraction"("guildId", "caseId");

-- CreateIndex
CREATE INDEX "UserLevel_guildId_xp_idx" ON "UserLevel"("guildId", "xp" DESC);

-- CreateIndex
CREATE UNIQUE INDEX "UserLevel_guildId_userId_key" ON "UserLevel"("guildId", "userId");

-- CreateIndex
CREATE INDEX "LevelRole_guildId_idx" ON "LevelRole"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "LevelRole_guildId_level_key" ON "LevelRole"("guildId", "level");

-- CreateIndex
CREATE INDEX "ReactionRoleGroup_guildId_idx" ON "ReactionRoleGroup"("guildId");

-- CreateIndex
CREATE INDEX "ReactionRoleGroup_messageId_idx" ON "ReactionRoleGroup"("messageId");

-- CreateIndex
CREATE UNIQUE INDEX "ReactionRoleItem_customId_key" ON "ReactionRoleItem"("customId");

-- CreateIndex
CREATE INDEX "ReactionRoleItem_groupId_idx" ON "ReactionRoleItem"("groupId");

-- CreateIndex
CREATE INDEX "CustomTag_guildId_idx" ON "CustomTag"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "CustomTag_guildId_name_key" ON "CustomTag"("guildId", "name");

-- CreateIndex
CREATE INDEX "AutoTrigger_guildId_idx" ON "AutoTrigger"("guildId");

-- CreateIndex
CREATE INDEX "AutoTrigger_guildId_enabled_idx" ON "AutoTrigger"("guildId", "enabled");

-- CreateIndex
CREATE INDEX "TranslateChannel_guildId_idx" ON "TranslateChannel"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "TranslateChannel_guildId_channelId_key" ON "TranslateChannel"("guildId", "channelId");

-- CreateIndex
CREATE INDEX "GuildMemberCache_guildId_inviterId_idx" ON "GuildMemberCache"("guildId", "inviterId");

-- CreateIndex
CREATE UNIQUE INDEX "GuildMemberCache_guildId_userId_key" ON "GuildMemberCache"("guildId", "userId");

-- CreateIndex
CREATE UNIQUE INDEX "UserEconomy_userId_key" ON "UserEconomy"("userId");

-- AddForeignKey
ALTER TABLE "Infraction" ADD CONSTRAINT "Infraction_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "UserLevel" ADD CONSTRAINT "UserLevel_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "LevelRole" ADD CONSTRAINT "LevelRole_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ReactionRoleGroup" ADD CONSTRAINT "ReactionRoleGroup_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ReactionRoleItem" ADD CONSTRAINT "ReactionRoleItem_groupId_fkey" FOREIGN KEY ("groupId") REFERENCES "ReactionRoleGroup"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "CustomTag" ADD CONSTRAINT "CustomTag_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "AutoTrigger" ADD CONSTRAINT "AutoTrigger_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "TranslateChannel" ADD CONSTRAINT "TranslateChannel_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "GuildMemberCache" ADD CONSTRAINT "GuildMemberCache_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

