//! `deps.botJoins` of dashboard.js: `import('./src/services/botJoinService.js')`, whose `listBotJoins` the server
//! calls for the "added by" line of the server list (feat-general's port).

use std::collections::HashMap;

use async_trait::async_trait;
use dash_server::{BotJoinRow, BotJoins};

/// botJoinService over the dashboard's database pool.
pub struct DbBotJoins {
    pool: bot_db::PgPool,
}

impl DbBotJoins {
    pub fn new(pool: bot_db::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BotJoins for DbBotJoins {
    async fn list_bot_joins(&self, guild_ids: &[String]) -> anyhow::Result<HashMap<String, BotJoinRow>> {
        let rows = feat_general::bot_join_service::list_bot_joins(&self.pool, guild_ids).await?;
        Ok(rows
            .into_iter()
            .map(|(g, r)| {
                // A Date the server writes with toISOString().
                let joined_at = bot_util::js::to_iso_string(r.joined_at.timestamp_millis() as f64).unwrap_or_default();
                (g, BotJoinRow { inviter_id: r.inviter_id, inviter_name: r.inviter_name, source: r.source, joined_at })
            })
            .collect())
    }
}
