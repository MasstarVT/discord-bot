-- CreateTable
CREATE TABLE "TranslateStat" (
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "via" TEXT NOT NULL,
    "direction" TEXT NOT NULL,
    "lang" TEXT NOT NULL,
    "count" INTEGER NOT NULL DEFAULT 0,
    "firstAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "lastAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "TranslateStat_pkey" PRIMARY KEY ("guildId","channelId","via","direction","lang")
);

-- AddForeignKey
ALTER TABLE "TranslateStat" ADD CONSTRAINT "TranslateStat_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

