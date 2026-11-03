-- AlterTable
ALTER TABLE "GuildSettings" ADD COLUMN     "captchaFallbackFormId" TEXT,
ADD COLUMN     "captchaRequiredRoleIds" TEXT[] DEFAULT ARRAY[]::TEXT[];
