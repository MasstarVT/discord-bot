-- AlterTable
ALTER TABLE "GuildEvent" ADD COLUMN     "accessRoleId" TEXT,
ADD COLUMN     "reminderMessageIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "textChannelId" TEXT,
ADD COLUMN     "voiceChannelId" TEXT;

-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "eventCategoryId" TEXT,
ADD COLUMN     "eventChannelsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "eventVoiceLeadMinutes" INTEGER NOT NULL DEFAULT 30;
