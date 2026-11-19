//! Port of `src/database/seed.js`: the development seed. Inserts a minimal `GuildSettings` row for local testing
//! (`TEST_GUILD_ID`, default `000000000000000000`). A binary calls [`seed`] for `npm run db:seed`.

use serde_json::json;
use sqlx::PgPool;

use crate::query::{InsertBuilder, SelectBuilder};
use crate::{DbError, GUILD_SETTINGS_INFO, GuildSettings};

/// What [`seed`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedOutcome {
    /// The row already existed.
    Skipped(String),
    /// The row was created.
    Seeded(String),
}

/// The guild the seed uses: `TEST_GUILD_ID` or `000000000000000000`.
pub fn test_guild_id() -> String {
    std::env::var("TEST_GUILD_ID").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "000000000000000000".to_string())
}

/// Creates the `GuildSettings` row of [`test_guild_id`] unless it exists, logging like seed.js.
pub async fn seed(pool: &PgPool) -> Result<SeedOutcome, DbError> {
    let guild_id = test_guild_id();
    let existing: Option<GuildSettings> =
        SelectBuilder::new(&GUILD_SETTINGS_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_optional(pool).await?;
    if existing.is_some() {
        tracing::info!("Seed skipped — GuildSettings already exists for {guild_id}");
        return Ok(SeedOutcome::Skipped(guild_id));
    }
    InsertBuilder::new(&GUILD_SETTINGS_INFO).value("guildId", json!(guild_id))?.without_returning().build()?.execute(pool).await?;
    tracing::info!("Seeded GuildSettings for guild {guild_id}");
    Ok(SeedOutcome::Seeded(guild_id))
}
