-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "countingChannelId" TEXT,
ADD COLUMN     "countingEnabled" BOOLEAN NOT NULL DEFAULT false;

-- CreateTable
CREATE TABLE "CountingState" (
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "count" INTEGER NOT NULL DEFAULT 0,
    "lastUserId" TEXT,
    "lastMessageId" TEXT,
    "best" INTEGER NOT NULL DEFAULT 0,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "CountingState_pkey" PRIMARY KEY ("guildId")
);

-- AddForeignKey
ALTER TABLE "CountingState" ADD CONSTRAINT "CountingState_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

