-- CreateEnum
CREATE TYPE "ModVisibility" AS ENUM ('PUBLIC', 'ANONYMOUS', 'PRIVATE');

-- CreateEnum
CREATE TYPE "AnnounceMode" AS ENUM ('OFF', 'CURRENT', 'DM', 'CHANNEL');

-- CreateEnum
CREATE TYPE "CaptchaMode" AS ENUM ('BUTTON', 'IMAGE');

-- CreateEnum
CREATE TYPE "ChannelListMode" AS ENUM ('ALLOW', 'DENY');

-- CreateEnum
CREATE TYPE "JobStatus" AS ENUM ('PENDING', 'RUNNING', 'DONE', 'FAILED', 'CANCELLED');

-- CreateEnum
CREATE TYPE "CooldownScope" AS ENUM ('NONE', 'SERVER', 'USER');

-- CreateEnum
CREATE TYPE "ReminderMode" AS ENUM ('TIME_OF_DAY', 'INTERVAL', 'ONE_SHOT');

-- CreateEnum
CREATE TYPE "GiveawayStatus" AS ENUM ('DRAFT', 'ACTIVE', 'ENDED', 'CANCELLED');

-- CreateEnum
CREATE TYPE "PollStatus" AS ENUM ('ACTIVE', 'ENDED');

-- CreateEnum
CREATE TYPE "TicketPanelStyle" AS ENUM ('BUTTONS', 'SELECT');

-- CreateEnum
CREATE TYPE "TicketStatus" AS ENUM ('OPEN', 'CLAIMED', 'CLOSED', 'DELETED');

-- CreateEnum
CREATE TYPE "StatKind" AS ENUM ('MEMBERS', 'HUMANS', 'BOTS', 'ONLINE', 'PEAK_ONLINE', 'TEXT_CHANNELS', 'VOICE_CHANNELS', 'TOTAL_CHANNELS', 'ROLES', 'ROLE_MEMBERS', 'BOOSTS');

-- CreateEnum
CREATE TYPE "AchievementTrigger" AS ENUM ('MESSAGES', 'REACTIONS', 'LEVEL', 'BOOST', 'PURCHASES');

-- CreateEnum
CREATE TYPE "AutomationTrigger" AS ENUM ('MESSAGE_SENT', 'MESSAGE_DELETED', 'MESSAGE_EDITED', 'BUTTON_CLICKED', 'REACTION_ADDED', 'ROLE_GAINED', 'ROLE_LOST', 'VOICE_JOINED', 'VOICE_LEFT', 'THREAD_CREATED', 'MEMBER_JOINED');

-- CreateEnum
CREATE TYPE "BoostPeriod" AS ENUM ('ONE_TIME', 'DAILY', 'WEEKLY', 'MONTHLY');

-- CreateEnum
CREATE TYPE "ShopItemType" AS ENUM ('ROLE', 'USABLE', 'STATIC');

-- CreateEnum
CREATE TYPE "FeedType" AS ENUM ('RSS', 'YOUTUBE', 'REDDIT', 'BLUESKY', 'PODCAST');

-- AlterEnum
ALTER TYPE "RRLayout" ADD VALUE 'REACTIONS';

-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "achievementAnnounceMode" "AnnounceMode" NOT NULL DEFAULT 'CURRENT',
ADD COLUMN     "achievementChannelId" TEXT,
ADD COLUMN     "achievementMessage" TEXT,
ADD COLUMN     "achievementUseEmbed" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "achievementsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "autoTriggersEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "automationsEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "birthdayChannelId" TEXT,
ADD COLUMN     "birthdayHour" INTEGER NOT NULL DEFAULT 9,
ADD COLUMN     "birthdayMessage" TEXT,
ADD COLUMN     "birthdayMessageAge" TEXT,
ADD COLUMN     "birthdayRequireYear" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "birthdayRoleId" TEXT,
ADD COLUMN     "birthdaysEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "captchaChannelId" TEXT,
ADD COLUMN     "captchaEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "captchaLockdownPrior" BOOLEAN,
ADD COLUMN     "captchaMessageId" TEXT,
ADD COLUMN     "captchaMode" "CaptchaMode" NOT NULL DEFAULT 'IMAGE',
ADD COLUMN     "captchaRoleId" TEXT,
ADD COLUMN     "captchaTemplate" JSONB,
ADD COLUMN     "captchaTimeoutMinutes" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "currencyEmoji" TEXT NOT NULL DEFAULT '🪙',
ADD COLUMN     "currencyName" TEXT NOT NULL DEFAULT 'coins',
ADD COLUMN     "customPrefix" TEXT NOT NULL DEFAULT '!',
ADD COLUMN     "dailyMax" INTEGER NOT NULL DEFAULT 120,
ADD COLUMN     "dailyMin" INTEGER NOT NULL DEFAULT 50,
ADD COLUMN     "disabledLogEvents" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "economyEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "economyGameChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "economyGameChannelMode" "ChannelListMode" NOT NULL DEFAULT 'ALLOW',
ADD COLUMN     "economyImportGlobal" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "economyMaxBalance" INTEGER NOT NULL DEFAULT 1000000000,
ADD COLUMN     "economyMaxBet" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "economyMaxPayout" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "economyStartingBalance" INTEGER NOT NULL DEFAULT 1000,
ADD COLUMN     "embedColor" INTEGER,
ADD COLUMN     "embedsEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "feedsEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "gamesEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "giveawaysEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "goodbyeChannelId" TEXT,
ADD COLUMN     "goodbyeEmbed" JSONB,
ADD COLUMN     "goodbyeEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "goodbyeMessage" TEXT,
ADD COLUMN     "goodbyeUseEmbed" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "immuneRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "invitesEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "levelRolesSyncOnDemotion" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "levelUpMode" "AnnounceMode" NOT NULL DEFAULT 'CURRENT',
ADD COLUMN     "levelUpUseEmbed" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "liveRoleEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "liveRoleId" TEXT,
ADD COLUMN     "liveRoleRequiredRoleId" TEXT,
ADD COLUMN     "logIgnoredChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "modDmTargets" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "modVisibility" "ModVisibility" NOT NULL DEFAULT 'PUBLIC',
ADD COLUMN     "pollsEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "rankCardStyle" JSONB,
ADD COLUMN     "remindersEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "searchEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "starboardEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "statsCategoryId" TEXT,
ADD COLUMN     "statsEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "tempVoiceEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "ticketsEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "timezone" TEXT NOT NULL DEFAULT 'UTC',
ADD COLUMN     "urbanNsfwOnly" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "welcomeCardStyle" JSONB,
ADD COLUMN     "welcomeDmCard" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "welcomeDmEmbed" JSONB,
ADD COLUMN     "welcomeDmEnabled" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "welcomeDmMessage" TEXT,
ADD COLUMN     "welcomeMessageEnabled" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "welcomeUseEmbed" BOOLEAN NOT NULL DEFAULT true,
ADD COLUMN     "workCooldownMinutes" INTEGER NOT NULL DEFAULT 60,
ADD COLUMN     "workMax" INTEGER NOT NULL DEFAULT 300,
ADD COLUMN     "workMin" INTEGER NOT NULL DEFAULT 100,
ADD COLUMN     "xpBoostRoles" JSONB,
ADD COLUMN     "xpMax" INTEGER NOT NULL DEFAULT 25,
ADD COLUMN     "xpMin" INTEGER NOT NULL DEFAULT 15,
ALTER COLUMN "modEnabled" SET DEFAULT true,
ALTER COLUMN "reactionRolesEnabled" SET DEFAULT true;

-- AlterTable
ALTER TABLE "ReactionRoleGroup" ADD COLUMN     "attached" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "messageContent" TEXT,
ADD COLUMN     "messageMissingAt" TIMESTAMP(3),
ADD COLUMN     "placeholder" TEXT,
ADD COLUMN     "plainText" BOOLEAN NOT NULL DEFAULT false;

-- AlterTable
ALTER TABLE "ReactionRoleItem" ADD COLUMN     "description" TEXT;

-- AlterTable
ALTER TABLE "CustomTag" ADD COLUMN     "actions" JSONB,
ADD COLUMN     "allowChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "allowRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "cooldownScope" "CooldownScope" NOT NULL DEFAULT 'NONE',
ADD COLUMN     "cooldownSeconds" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "deleteInvocation" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "denyChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "denyRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
ADD COLUMN     "description" TEXT,
ADD COLUMN     "enabled" BOOLEAN NOT NULL DEFAULT true;

-- AlterTable
ALTER TABLE "GuildMemberCache" ADD COLUMN     "leftAt" TIMESTAMP(3);

-- CreateTable
CREATE TABLE "ScheduledJob" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "type" TEXT NOT NULL,
    "dedupeKey" TEXT,
    "runAt" TIMESTAMP(3) NOT NULL,
    "payload" JSONB NOT NULL DEFAULT '{}',
    "status" "JobStatus" NOT NULL DEFAULT 'PENDING',
    "attempts" INTEGER NOT NULL DEFAULT 0,
    "maxAttempts" INTEGER NOT NULL DEFAULT 5,
    "lastError" TEXT,
    "lockedBy" TEXT,
    "lockedUntil" TIMESTAMP(3),
    "version" INTEGER NOT NULL DEFAULT 0,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "ScheduledJob_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "PersonalInvite" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "code" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "PersonalInvite_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "SavedEmbed" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "template" JSONB NOT NULL,
    "channelId" TEXT,
    "messageId" TEXT,
    "createdBy" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "SavedEmbed_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Reminder" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "template" JSONB NOT NULL,
    "mode" "ReminderMode" NOT NULL,
    "weekdays" INTEGER[] DEFAULT ARRAY[]::INTEGER[],
    "timeOfDay" TEXT,
    "intervalMinutes" INTEGER,
    "windowStart" TEXT,
    "windowEnd" TEXT,
    "runAt" TIMESTAMP(3),
    "requireActivity" BOOLEAN NOT NULL DEFAULT false,
    "pingRoleId" TEXT,
    "lastSentAt" TIMESTAMP(3),
    "lastMessageId" TEXT,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdBy" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Reminder_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Giveaway" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "messageId" TEXT,
    "hostId" TEXT NOT NULL,
    "prize" TEXT NOT NULL,
    "description" TEXT,
    "winnerCount" INTEGER NOT NULL DEFAULT 1,
    "requiredRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "deniedRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "roleWeights" JSONB,
    "xpPrize" INTEGER,
    "coinPrize" INTEGER,
    "pingRoleId" TEXT,
    "embedColor" INTEGER,
    "endsAt" TIMESTAMP(3) NOT NULL,
    "endedAt" TIMESTAMP(3),
    "status" "GiveawayStatus" NOT NULL DEFAULT 'ACTIVE',
    "winnerIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Giveaway_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "GiveawayEntry" (
    "id" TEXT NOT NULL,
    "giveawayId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "GiveawayEntry_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Poll" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "messageId" TEXT,
    "authorId" TEXT NOT NULL,
    "question" TEXT NOT NULL,
    "options" TEXT[],
    "maxChoices" INTEGER NOT NULL DEFAULT 1,
    "allowedRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "deniedRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "hideResults" BOOLEAN NOT NULL DEFAULT false,
    "endsAt" TIMESTAMP(3),
    "status" "PollStatus" NOT NULL DEFAULT 'ACTIVE',
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Poll_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "PollVote" (
    "id" TEXT NOT NULL,
    "pollId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "optionIndex" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "PollVote_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Starboard" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "emojis" TEXT[] DEFAULT ARRAY['⭐']::TEXT[],
    "threshold" INTEGER NOT NULL DEFAULT 3,
    "selfStar" BOOLEAN NOT NULL DEFAULT false,
    "ignoreBots" BOOLEAN NOT NULL DEFAULT true,
    "removeBelowThreshold" BOOLEAN NOT NULL DEFAULT true,
    "autoReact" BOOLEAN NOT NULL DEFAULT false,
    "maxMessageAgeDays" INTEGER NOT NULL DEFAULT 0,
    "allowNsfw" BOOLEAN NOT NULL DEFAULT false,
    "ignoredChannelIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "removeSelfStars" BOOLEAN NOT NULL DEFAULT false,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Starboard_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "StarboardEntry" (
    "id" TEXT NOT NULL,
    "starboardId" TEXT NOT NULL,
    "sourceChannelId" TEXT NOT NULL,
    "sourceMessageId" TEXT NOT NULL,
    "authorId" TEXT NOT NULL,
    "boardMessageId" TEXT,
    "count" INTEGER NOT NULL DEFAULT 0,
    "blocked" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "StarboardEntry_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "TicketPanel" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "channelId" TEXT,
    "messageId" TEXT,
    "template" JSONB,
    "style" "TicketPanelStyle" NOT NULL DEFAULT 'BUTTONS',
    "managerRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "openCategoryId" TEXT,
    "claimedCategoryId" TEXT,
    "closedCategoryId" TEXT,
    "introTemplate" JSONB,
    "transcriptChannelId" TEXT,
    "dmTranscript" BOOLEAN NOT NULL DEFAULT false,
    "maxOpenPerUser" INTEGER NOT NULL DEFAULT 1,
    "channelNameFormat" TEXT NOT NULL DEFAULT 'ticket-{number}',
    "askReason" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "TicketPanel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "TicketPanelOption" (
    "id" TEXT NOT NULL,
    "panelId" TEXT NOT NULL,
    "label" TEXT NOT NULL,
    "emoji" TEXT,
    "description" TEXT,
    "buttonStyle" INTEGER NOT NULL DEFAULT 1,
    "position" INTEGER NOT NULL DEFAULT 0,
    "introTemplate" JSONB,
    "categoryId" TEXT,

    CONSTRAINT "TicketPanelOption_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Ticket" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "panelId" TEXT,
    "optionId" TEXT,
    "number" INTEGER NOT NULL,
    "channelId" TEXT NOT NULL,
    "ownerId" TEXT NOT NULL,
    "status" "TicketStatus" NOT NULL DEFAULT 'OPEN',
    "claimedById" TEXT,
    "closedById" TEXT,
    "closedAt" TIMESTAMP(3),
    "reason" TEXT,
    "addedUserIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Ticket_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "TempVoiceHub" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "categoryId" TEXT,
    "nameFormat" TEXT NOT NULL DEFAULT '#{index} - {username}''s Channel',
    "userLimit" INTEGER NOT NULL DEFAULT 0,
    "bitrate" INTEGER,
    "keepAliveMinutes" INTEGER NOT NULL DEFAULT 0,
    "ownershipLockMinutes" INTEGER NOT NULL DEFAULT 0,
    "syncPermissions" BOOLEAN NOT NULL DEFAULT true,
    "createTextChannel" BOOLEAN NOT NULL DEFAULT false,
    "textChannelMembersOnly" BOOLEAN NOT NULL DEFAULT true,
    "ignoredRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "moderatorRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "TempVoiceHub_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "TempVoiceChannel" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "hubId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "textChannelId" TEXT,
    "controlMessageId" TEXT,
    "ownerId" TEXT NOT NULL,
    "index" INTEGER NOT NULL,
    "locked" BOOLEAN NOT NULL DEFAULT false,
    "hidden" BOOLEAN NOT NULL DEFAULT false,
    "bannedUserIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "ownerLeftAt" TIMESTAMP(3),
    "emptySince" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "TempVoiceChannel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "StatChannel" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "kind" "StatKind" NOT NULL,
    "roleId" TEXT,
    "template" TEXT NOT NULL,
    "lastValue" INTEGER,
    "lastRenamedAt" TIMESTAMP(3),
    "peakValue" INTEGER,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "StatChannel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Birthday" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "month" INTEGER NOT NULL,
    "day" INTEGER NOT NULL,
    "year" INTEGER,
    "lastAnnouncedYear" INTEGER,
    "roleGrantedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Birthday_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Achievement" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "description" TEXT,
    "emoji" TEXT,
    "trigger" "AchievementTrigger" NOT NULL,
    "tiers" JSONB NOT NULL,
    "trackPastProgress" BOOLEAN NOT NULL DEFAULT true,
    "deadline" TIMESTAMP(3),
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Achievement_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "AchievementProgress" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "achievementId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "progress" INTEGER NOT NULL DEFAULT 0,
    "tier" INTEGER NOT NULL DEFAULT 0,
    "almostNotified" BOOLEAN NOT NULL DEFAULT false,
    "completedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "AchievementProgress_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Automation" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "trigger" "AutomationTrigger" NOT NULL,
    "conditions" JSONB NOT NULL DEFAULT '[]',
    "actions" JSONB NOT NULL DEFAULT '[]',
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "cooldownSeconds" INTEGER NOT NULL DEFAULT 0,
    "buttonSeq" INTEGER NOT NULL DEFAULT 0,
    "uses" INTEGER NOT NULL DEFAULT 0,
    "createdBy" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Automation_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "EconomyAccount" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "balance" INTEGER NOT NULL DEFAULT 0,
    "dailyStreak" INTEGER NOT NULL DEFAULT 0,
    "lastDaily" TIMESTAMP(3),
    "lastWork" TIMESTAMP(3),
    "lastPity" TIMESTAMP(3),
    "importedGlobal" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "EconomyAccount_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "EconomyBoost" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "roleId" TEXT,
    "percent" INTEGER NOT NULL,
    "period" "BoostPeriod" NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdBy" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "EconomyBoost_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "EconomyBoostUse" (
    "id" TEXT NOT NULL,
    "boostId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "periodKey" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "EconomyBoostUse_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "ShopItem" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "description" TEXT,
    "price" INTEGER NOT NULL,
    "type" "ShopItemType" NOT NULL DEFAULT 'STATIC',
    "roleId" TEXT,
    "imageUrl" TEXT,
    "useOnBuy" BOOLEAN NOT NULL DEFAULT false,
    "destroyOnUse" BOOLEAN NOT NULL DEFAULT true,
    "useMessage" TEXT,
    "buyMessage" TEXT,
    "canBuyMultiple" BOOLEAN NOT NULL DEFAULT true,
    "maxPerUser" INTEGER NOT NULL DEFAULT 200,
    "stock" INTEGER,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "ShopItem_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "InventoryItem" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "itemId" TEXT NOT NULL,
    "quantity" INTEGER NOT NULL DEFAULT 0,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "InventoryItem_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "Feed" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "type" "FeedType" NOT NULL,
    "source" TEXT NOT NULL,
    "displayName" TEXT NOT NULL,
    "fetchUrl" TEXT NOT NULL,
    "channelId" TEXT NOT NULL,
    "pingRoleId" TEXT,
    "messageTemplate" TEXT,
    "includeReposts" BOOLEAN NOT NULL DEFAULT false,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "initialized" BOOLEAN NOT NULL DEFAULT false,
    "seenIds" TEXT[] DEFAULT ARRAY[]::TEXT[],
    "etag" TEXT,
    "lastModified" TEXT,
    "lastCheckedAt" TIMESTAMP(3),
    "lastSuccessAt" TIMESTAMP(3),
    "lastPostedAt" TIMESTAMP(3),
    "failureCount" INTEGER NOT NULL DEFAULT 0,
    "lastError" TEXT,
    "disabledReason" TEXT,
    "createdBy" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Feed_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "CaptchaVerification" (
    "id" TEXT NOT NULL,
    "guildId" TEXT NOT NULL,
    "userId" TEXT NOT NULL,
    "answerHash" TEXT,
    "attempts" INTEGER NOT NULL DEFAULT 0,
    "expiresAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "CaptchaVerification_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "ScheduledJob_dedupeKey_key" ON "ScheduledJob"("dedupeKey");

-- CreateIndex
CREATE INDEX "ScheduledJob_status_runAt_idx" ON "ScheduledJob"("status", "runAt");

-- CreateIndex
CREATE INDEX "ScheduledJob_guildId_type_idx" ON "ScheduledJob"("guildId", "type");

-- CreateIndex
CREATE UNIQUE INDEX "PersonalInvite_code_key" ON "PersonalInvite"("code");

-- CreateIndex
CREATE UNIQUE INDEX "PersonalInvite_guildId_userId_key" ON "PersonalInvite"("guildId", "userId");

-- CreateIndex
CREATE INDEX "SavedEmbed_messageId_idx" ON "SavedEmbed"("messageId");

-- CreateIndex
CREATE UNIQUE INDEX "SavedEmbed_guildId_name_key" ON "SavedEmbed"("guildId", "name");

-- CreateIndex
CREATE INDEX "Reminder_guildId_channelId_idx" ON "Reminder"("guildId", "channelId");

-- CreateIndex
CREATE UNIQUE INDEX "Reminder_guildId_name_key" ON "Reminder"("guildId", "name");

-- CreateIndex
CREATE INDEX "Giveaway_guildId_status_idx" ON "Giveaway"("guildId", "status");

-- CreateIndex
CREATE INDEX "Giveaway_messageId_idx" ON "Giveaway"("messageId");

-- CreateIndex
CREATE UNIQUE INDEX "GiveawayEntry_giveawayId_userId_key" ON "GiveawayEntry"("giveawayId", "userId");

-- CreateIndex
CREATE INDEX "Poll_guildId_status_idx" ON "Poll"("guildId", "status");

-- CreateIndex
CREATE INDEX "Poll_messageId_idx" ON "Poll"("messageId");

-- CreateIndex
CREATE INDEX "PollVote_pollId_idx" ON "PollVote"("pollId");

-- CreateIndex
CREATE UNIQUE INDEX "PollVote_pollId_userId_optionIndex_key" ON "PollVote"("pollId", "userId", "optionIndex");

-- CreateIndex
CREATE INDEX "Starboard_guildId_idx" ON "Starboard"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "Starboard_guildId_name_key" ON "Starboard"("guildId", "name");

-- CreateIndex
CREATE INDEX "StarboardEntry_sourceMessageId_idx" ON "StarboardEntry"("sourceMessageId");

-- CreateIndex
CREATE INDEX "StarboardEntry_boardMessageId_idx" ON "StarboardEntry"("boardMessageId");

-- CreateIndex
CREATE UNIQUE INDEX "StarboardEntry_starboardId_sourceMessageId_key" ON "StarboardEntry"("starboardId", "sourceMessageId");

-- CreateIndex
CREATE UNIQUE INDEX "TicketPanel_guildId_name_key" ON "TicketPanel"("guildId", "name");

-- CreateIndex
CREATE INDEX "TicketPanelOption_panelId_idx" ON "TicketPanelOption"("panelId");

-- CreateIndex
CREATE UNIQUE INDEX "Ticket_channelId_key" ON "Ticket"("channelId");

-- CreateIndex
CREATE INDEX "Ticket_guildId_ownerId_status_idx" ON "Ticket"("guildId", "ownerId", "status");

-- CreateIndex
CREATE UNIQUE INDEX "Ticket_guildId_number_key" ON "Ticket"("guildId", "number");

-- CreateIndex
CREATE UNIQUE INDEX "TempVoiceHub_channelId_key" ON "TempVoiceHub"("channelId");

-- CreateIndex
CREATE INDEX "TempVoiceHub_guildId_idx" ON "TempVoiceHub"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "TempVoiceChannel_channelId_key" ON "TempVoiceChannel"("channelId");

-- CreateIndex
CREATE INDEX "TempVoiceChannel_guildId_idx" ON "TempVoiceChannel"("guildId");

-- CreateIndex
CREATE INDEX "TempVoiceChannel_hubId_idx" ON "TempVoiceChannel"("hubId");

-- CreateIndex
CREATE UNIQUE INDEX "StatChannel_channelId_key" ON "StatChannel"("channelId");

-- CreateIndex
CREATE INDEX "StatChannel_guildId_idx" ON "StatChannel"("guildId");

-- CreateIndex
CREATE INDEX "Birthday_guildId_month_day_idx" ON "Birthday"("guildId", "month", "day");

-- CreateIndex
CREATE UNIQUE INDEX "Birthday_guildId_userId_key" ON "Birthday"("guildId", "userId");

-- CreateIndex
CREATE INDEX "Achievement_guildId_trigger_enabled_idx" ON "Achievement"("guildId", "trigger", "enabled");

-- CreateIndex
CREATE UNIQUE INDEX "Achievement_guildId_name_key" ON "Achievement"("guildId", "name");

-- CreateIndex
CREATE INDEX "AchievementProgress_guildId_userId_idx" ON "AchievementProgress"("guildId", "userId");

-- CreateIndex
CREATE UNIQUE INDEX "AchievementProgress_achievementId_userId_key" ON "AchievementProgress"("achievementId", "userId");

-- CreateIndex
CREATE INDEX "Automation_guildId_trigger_enabled_idx" ON "Automation"("guildId", "trigger", "enabled");

-- CreateIndex
CREATE UNIQUE INDEX "Automation_guildId_name_key" ON "Automation"("guildId", "name");

-- CreateIndex
CREATE INDEX "EconomyAccount_guildId_balance_idx" ON "EconomyAccount"("guildId", "balance" DESC);

-- CreateIndex
CREATE UNIQUE INDEX "EconomyAccount_guildId_userId_key" ON "EconomyAccount"("guildId", "userId");

-- CreateIndex
CREATE UNIQUE INDEX "EconomyBoost_guildId_name_key" ON "EconomyBoost"("guildId", "name");

-- CreateIndex
CREATE UNIQUE INDEX "EconomyBoostUse_boostId_userId_periodKey_key" ON "EconomyBoostUse"("boostId", "userId", "periodKey");

-- CreateIndex
CREATE UNIQUE INDEX "ShopItem_guildId_name_key" ON "ShopItem"("guildId", "name");

-- CreateIndex
CREATE INDEX "InventoryItem_guildId_userId_idx" ON "InventoryItem"("guildId", "userId");

-- CreateIndex
CREATE UNIQUE INDEX "InventoryItem_itemId_userId_key" ON "InventoryItem"("itemId", "userId");

-- CreateIndex
CREATE INDEX "Feed_type_source_idx" ON "Feed"("type", "source");

-- CreateIndex
CREATE INDEX "Feed_guildId_idx" ON "Feed"("guildId");

-- CreateIndex
CREATE UNIQUE INDEX "Feed_guildId_type_source_channelId_key" ON "Feed"("guildId", "type", "source", "channelId");

-- CreateIndex
CREATE UNIQUE INDEX "CaptchaVerification_guildId_userId_key" ON "CaptchaVerification"("guildId", "userId");

-- AddForeignKey
ALTER TABLE "ScheduledJob" ADD CONSTRAINT "ScheduledJob_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "PersonalInvite" ADD CONSTRAINT "PersonalInvite_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "SavedEmbed" ADD CONSTRAINT "SavedEmbed_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Reminder" ADD CONSTRAINT "Reminder_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Giveaway" ADD CONSTRAINT "Giveaway_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "GiveawayEntry" ADD CONSTRAINT "GiveawayEntry_giveawayId_fkey" FOREIGN KEY ("giveawayId") REFERENCES "Giveaway"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Poll" ADD CONSTRAINT "Poll_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "PollVote" ADD CONSTRAINT "PollVote_pollId_fkey" FOREIGN KEY ("pollId") REFERENCES "Poll"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Starboard" ADD CONSTRAINT "Starboard_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "StarboardEntry" ADD CONSTRAINT "StarboardEntry_starboardId_fkey" FOREIGN KEY ("starboardId") REFERENCES "Starboard"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "TicketPanel" ADD CONSTRAINT "TicketPanel_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "TicketPanelOption" ADD CONSTRAINT "TicketPanelOption_panelId_fkey" FOREIGN KEY ("panelId") REFERENCES "TicketPanel"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Ticket" ADD CONSTRAINT "Ticket_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Ticket" ADD CONSTRAINT "Ticket_panelId_fkey" FOREIGN KEY ("panelId") REFERENCES "TicketPanel"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Ticket" ADD CONSTRAINT "Ticket_optionId_fkey" FOREIGN KEY ("optionId") REFERENCES "TicketPanelOption"("id") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "TempVoiceHub" ADD CONSTRAINT "TempVoiceHub_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "TempVoiceChannel" ADD CONSTRAINT "TempVoiceChannel_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "TempVoiceChannel" ADD CONSTRAINT "TempVoiceChannel_hubId_fkey" FOREIGN KEY ("hubId") REFERENCES "TempVoiceHub"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "StatChannel" ADD CONSTRAINT "StatChannel_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Birthday" ADD CONSTRAINT "Birthday_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Achievement" ADD CONSTRAINT "Achievement_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "AchievementProgress" ADD CONSTRAINT "AchievementProgress_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "AchievementProgress" ADD CONSTRAINT "AchievementProgress_achievementId_fkey" FOREIGN KEY ("achievementId") REFERENCES "Achievement"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Automation" ADD CONSTRAINT "Automation_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "EconomyAccount" ADD CONSTRAINT "EconomyAccount_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "EconomyBoost" ADD CONSTRAINT "EconomyBoost_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "EconomyBoostUse" ADD CONSTRAINT "EconomyBoostUse_boostId_fkey" FOREIGN KEY ("boostId") REFERENCES "EconomyBoost"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "ShopItem" ADD CONSTRAINT "ShopItem_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "InventoryItem" ADD CONSTRAINT "InventoryItem_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "InventoryItem" ADD CONSTRAINT "InventoryItem_itemId_fkey" FOREIGN KEY ("itemId") REFERENCES "ShopItem"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "Feed" ADD CONSTRAINT "Feed_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "CaptchaVerification" ADD CONSTRAINT "CaptchaVerification_guildId_fkey" FOREIGN KEY ("guildId") REFERENCES "GuildSettings"("guildId") ON DELETE CASCADE ON UPDATE CASCADE;


-- ─────────────────────────────────────────────────────────────────────────────
-- Data migration (hand-written; keep below the generated DDL).
-- Only enums created above with CREATE TYPE are referenced; never RRLayout.REACTIONS.
-- Order matters: autoTriggersEnabled is copied before customCommandsEnabled changes.
-- ─────────────────────────────────────────────────────────────────────────────

-- Modules that already worked without a toggle stay on now that the flags gate them.
UPDATE "GuildSettings" SET "modEnabled" = true, "reactionRolesEnabled" = true;

-- At HEAD customCommandsEnabled gated ONLY the /trigger auto-responders (/trigger enable|disable).
-- Keep that on/off state in the new sub-switch for guilds that have triggers (others keep the default true).
UPDATE "GuildSettings" g SET "autoTriggersEnabled" = g."customCommandsEnabled"
WHERE EXISTS (SELECT 1 FROM "AutoTrigger" a WHERE a."guildId" = g."guildId");

-- /tag ignored customCommandsEnabled before (R101); keep tags working where they exist.
UPDATE "GuildSettings" g SET "customCommandsEnabled" = true
WHERE EXISTS (SELECT 1 FROM "CustomTag" t WHERE t."guildId" = g."guildId");

-- The legacy level-up token {user} meant the username; MEE6 {user} is a mention.
UPDATE "GuildSettings" SET "levelUpMessage" = replace("levelUpMessage", '{user}', '{user.name}')
WHERE "levelUpMessage" LIKE '%{user}%';

-- Every legacy level-up was an embed (so {mention} never pinged); keep that for upgraded guilds.
-- New guilds keep the MEE6 plain-text default (false).
UPDATE "GuildSettings" SET "levelUpUseEmbed" = true;

-- A configured level-up channel meant "custom channel" mode.
UPDATE "GuildSettings" SET "levelUpMode" = 'CHANNEL' WHERE "levelUpChannelId" IS NOT NULL;

-- Preserve customised XP rates (legacy range was rate ± floor(rate * 0.2)).
UPDATE "GuildSettings"
SET "xpMin" = GREATEST(1, "xpRate" - FLOOR("xpRate" * 0.2)::int),
    "xpMax" = GREATEST(1, "xpRate" + FLOOR("xpRate" * 0.2)::int)
WHERE "xpRate" <> 15;

-- WARN infractions were stored inactive by a bug (R041/R042/R148/R149); they count until pardoned.
UPDATE "Infraction" SET "active" = true WHERE "type" = 'WARN' AND "active" = false;

-- Legacy escalation tiers never fired (WARNs were inactive, so nothing counted). Stamp countFrom with the
-- migration time on every legacy tier so that warnings given BEFORE this upgrade never trigger them
-- (pkg-moderation.md §3.2). Tiers that already carry countFrom are left alone.
UPDATE "GuildSettings"
SET "infractionEscalation" = (
  SELECT jsonb_agg(
           CASE WHEN jsonb_typeof(t.elem) = 'object' AND (t.elem -> 'countFrom') IS NULL
                THEN t.elem || jsonb_build_object('countFrom',
                       to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                ELSE t.elem END
           ORDER BY t.ord)
  FROM jsonb_array_elements("infractionEscalation") WITH ORDINALITY AS t(elem, ord)
)
WHERE jsonb_typeof("infractionEscalation") = 'array'
  AND jsonb_array_length("infractionEscalation") > 0;
