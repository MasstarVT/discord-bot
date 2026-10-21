-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "liveRoleAnnounceChannelId" TEXT,
ADD COLUMN     "liveRoleAnnounceMessage" TEXT,
ADD COLUMN     "liveRoleAnnouncePingRoleId" TEXT;
