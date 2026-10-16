-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN "timezoneRoles" JSONB;

-- CreateTable
CREATE TABLE "UserTimezone" (
    "userId" TEXT NOT NULL,
    "zone" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "UserTimezone_pkey" PRIMARY KEY ("userId")
);
