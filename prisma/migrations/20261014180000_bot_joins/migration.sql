-- CreateTable
CREATE TABLE "BotJoin" (
    "guildId" TEXT NOT NULL,
    "inviterId" TEXT,
    "inviterName" TEXT,
    "source" TEXT,
    "joinedAt" TIMESTAMP(3) NOT NULL,
    "checkedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "BotJoin_pkey" PRIMARY KEY ("guildId")
);

-- AddForeignKey
ALTER TABLE "BotJoin" ADD CONSTRAINT "BotJoin_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;
