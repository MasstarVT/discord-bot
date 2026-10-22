-- CreateEnum
CREATE TYPE "ConfessionStatus" AS ENUM ('PENDING', 'APPROVED', 'REJECTED', 'DELETED');

-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "autoPublishChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "autoThreadChannels" JSONB NOT NULL DEFAULT '[]',
ADD COLUMN     "autoThreadsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "backupIntervalHours" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "backupKeep" INTEGER NOT NULL DEFAULT 7,
ADD COLUMN     "backupsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "boostChannelId" TEXT,
ADD COLUMN     "boostCustomRoles" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "boostMessage" TEXT,
ADD COLUMN     "boostRoleAnchorId" TEXT,
ADD COLUMN     "boostRoleGraceDays" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "boostsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "bumpChannelId" TEXT,
ADD COLUMN     "bumpReminderMessage" TEXT,
ADD COLUMN     "bumpRewardCoins" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "bumpRoleId" TEXT,
ADD COLUMN     "bumpThanks" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "bumpThanksMessage" TEXT,
ADD COLUMN     "bumpsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "confessionAllowImages" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "confessionBlockLinks" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "confessionBlockedUserIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "confessionChannelId" TEXT,
ADD COLUMN     "confessionCooldownSeconds" INTEGER NOT NULL DEFAULT 300,
ADD COLUMN     "confessionReviewChannelId" TEXT,
ADD COLUMN     "confessionsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "voiceRoleAnyRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "voiceRoleIgnoreAfk" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "voiceRoleLinks" JSONB NOT NULL DEFAULT '[]',
ADD COLUMN     "voiceRolesEnabled" BOOLEAN NOT NULL DEFAULT false;

-- CreateTable
CREATE TABLE "BoosterRole" (
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "roleId" TEXT NOT NULL,
    "boostEndedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "BoosterRole_pkey" PRIMARY KEY ("guildId","userId")
);

-- CreateTable
CREATE TABLE "ServerBackup" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT,
    "automatic" BOOLEAN NOT NULL DEFAULT false,
    "createdById" TEXT,
    "data" JSONB NOT NULL,
    "sizeBytes" INTEGER NOT NULL,
    "roleCount" INTEGER NOT NULL,
    "channelCount" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "ServerBackup_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Confession" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "number" INTEGER NOT NULL,
    "authorId" TEXT NOT NULL,
    "content" TEXT NOT NULL,
    "hasImage" BOOLEAN NOT NULL DEFAULT false,
    "status" "ConfessionStatus" NOT NULL DEFAULT 'PENDING',
    "channelId" TEXT,
    "messageId" TEXT,
    "reviewMessageId" TEXT,
    "reviewerId" TEXT,
    "reviewedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "Confession_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "BumpState" (
    "guildId" TEXT NOT NULL,
    "site" TEXT NOT NULL,
    "lastBumpAt" TIMESTAMP(3) NOT NULL,
    "lastBumperId" TEXT,
    "channelId" TEXT NOT NULL,
    "nextBumpAt" TIMESTAMP(3) NOT NULL,
    "remindedAt" TIMESTAMP(3),

    CONSTRAINT "BumpState_pkey" PRIMARY KEY ("guildId","site")
);

-- CreateTable
CREATE TABLE "BumpCount" (
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "count" INTEGER NOT NULL DEFAULT 0,
    "lastBumpAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "BumpCount_pkey" PRIMARY KEY ("guildId","userId")
);

-- CreateIndex
CREATE UNIQUE INDEX "BoosterRole_roleId_key" ON "BoosterRole"("roleId");

-- CreateIndex
CREATE INDEX "BoosterRole_guildId_boostEndedAt_idx" ON "BoosterRole"("guildId", "boostEndedAt");

-- CreateIndex
CREATE INDEX "ServerBackup_guildId_automatic_createdAt_idx" ON "ServerBackup"("guildId", "automatic", "createdAt");

-- CreateIndex
CREATE UNIQUE INDEX "Confession_messageId_key" ON "Confession"("messageId");

-- CreateIndex
CREATE UNIQUE INDEX "Confession_reviewMessageId_key" ON "Confession"("reviewMessageId");

-- CreateIndex
CREATE INDEX "Confession_guildId_status_idx" ON "Confession"("guildId", "status");

-- CreateIndex
CREATE INDEX "Confession_guildId_authorId_createdAt_idx" ON "Confession"("guildId", "authorId", "createdAt");

-- CreateIndex
CREATE UNIQUE INDEX "Confession_guildId_number_key" ON "Confession"("guildId", "number");

-- CreateIndex
CREATE INDEX "BumpCount_guildId_count_idx" ON "BumpCount"("guildId", "count");

-- AddForeignKey
ALTER TABLE "BoosterRole" ADD CONSTRAINT "BoosterRole_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ServerBackup" ADD CONSTRAINT "ServerBackup_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Confession" ADD CONSTRAINT "Confession_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "BumpState" ADD CONSTRAINT "BumpState_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "BumpCount" ADD CONSTRAINT "BumpCount_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

