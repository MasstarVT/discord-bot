-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "autoTimeEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "autoTimeIgnoredChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[];
