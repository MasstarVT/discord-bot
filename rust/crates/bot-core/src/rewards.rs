//! Port of `src/services/rewards.js`: the XP and coins provider bus.
//!
//! Giveaways, achievements and automations grant XP and coins through providers that the levels and economy plugins
//! register (`Plugin::provides`), so they never depend on those crates. [`RewardsBus::grant`] / [`RewardsBus::take`]
//! never fail and answer `{ ok, total?, reason? }` (`reason`: `invalid-amount` unless an integer 1 to 1e9,
//! `unavailable`, `error`, or whatever the provider says, e.g. `module-disabled`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bot_config::ModuleKey;
use bot_db::GuildSettings;
use serde::{Deserialize, Serialize};
use serenity::all::{ChannelId, GuildId, Member};

use crate::context::App;
use crate::error::{BotResult, guard, log_arg};
use crate::settings::is_module_enabled;

/// The largest amount one call may grant or take.
pub const MAX_AMOUNT: i64 = 1_000_000_000;

/// A reward kind (`REWARD_KINDS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RewardKind {
    Xp,
    Coins,
}

impl RewardKind {
    /// `REWARD_KINDS`, in JS order.
    pub const ALL: [RewardKind; 2] = [RewardKind::Xp, RewardKind::Coins];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xp => "xp",
            Self::Coins => "coins",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// `REWARD_MODULES[kind]` / `providerModule(kind)`: the module that backs the kind.
    pub fn module(self) -> ModuleKey {
        match self {
            Self::Xp => ModuleKey::Leveling,
            Self::Coins => ModuleKey::Economy,
        }
    }
}

impl std::fmt::Display for RewardKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `providerModule(kind)` by name: `None` for an unknown kind.
pub fn provider_module(kind: &str) -> Option<ModuleKey> {
    RewardKind::from_name(kind).map(RewardKind::module)
}

/// The argument object of `grant` / `take`.
#[derive(Debug, Clone)]
pub struct RewardArgs {
    pub guild_id: GuildId,
    pub user_id: String,
    pub amount: i64,
    /// Default `''`.
    pub reason: String,
    pub member: Option<Member>,
    pub channel_id: Option<ChannelId>,
}

impl RewardArgs {
    pub fn new(guild_id: GuildId, user_id: impl Into<String>, amount: i64) -> Self {
        Self { guild_id, user_id: user_id.into(), amount, reason: String::new(), member: None, channel_id: None }
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    pub fn member(mut self, member: Option<Member>) -> Self {
        self.member = member;
        self
    }

    pub fn channel(mut self, channel_id: Option<ChannelId>) -> Self {
        self.channel_id = channel_id;
        self
    }
}

/// `{ ok, total?, reason? }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub total: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

impl RewardResult {
    pub fn ok(total: Option<i64>) -> Self {
        Self { ok: true, total, reason: None }
    }

    pub fn fail(reason: &str) -> Self {
        Self { ok: false, total: None, reason: Some(reason.to_string()) }
    }
}

/// A provider: the levels plugin for `xp`, the economy plugin for `coins`. Providers enforce their own module toggle
/// (`module-disabled`) and caps.
#[async_trait]
pub trait RewardProvider: Send + Sync + 'static {
    async fn grant(&self, app: &App, args: RewardArgs) -> BotResult<RewardResult>;
    async fn take(&self, app: &App, args: RewardArgs) -> BotResult<RewardResult>;
}

struct Entry {
    provider: Arc<dyn RewardProvider>,
    owner: Option<String>,
}

/// The bus. One per app.
#[derive(Default)]
pub struct RewardsBus {
    providers: RwLock<HashMap<RewardKind, Entry>>,
}

impl RewardsBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// `registerProvider(kind, impl, ownerPluginId)`: a second registration is refused.
    pub fn register_provider(&self, kind: RewardKind, provider: Arc<dyn RewardProvider>, owner: Option<&str>) -> Result<(), String> {
        let mut map = self.providers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(&kind) {
            return Err(format!(
                "Reward provider for \"{kind}\" is already registered by {}",
                existing.owner.as_deref().unwrap_or("another plugin")
            ));
        }
        map.insert(kind, Entry { provider, owner: owner.map(str::to_string) });
        Ok(())
    }

    /// `hasProvider(kind)`.
    pub fn has_provider(&self, kind: RewardKind) -> bool {
        self.providers.read().unwrap_or_else(|e| e.into_inner()).contains_key(&kind)
    }

    /// `isRewardAvailable(kind, settings)`: synchronous; a provider is registered and its module is on.
    pub fn is_reward_available(&self, kind: RewardKind, settings: Option<&GuildSettings>) -> bool {
        self.has_provider(kind) && is_module_enabled(settings, Some(kind.module()))
    }

    async fn dispatch(&self, app: &App, grant: bool, kind: RewardKind, args: RewardArgs) -> RewardResult {
        if args.amount < 1 || args.amount > MAX_AMOUNT {
            return RewardResult::fail("invalid-amount");
        }
        let provider = match self.providers.read().unwrap_or_else(|e| e.into_inner()).get(&kind) {
            Some(e) => e.provider.clone(),
            None => return RewardResult::fail("unavailable"),
        };
        let method = if grant { "grant" } else { "take" };
        let app = app.clone();
        let result = guard(async move { if grant { provider.grant(&app, args).await } else { provider.take(&app, args).await } }).await;
        match result {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!(format!("Reward provider \"{kind}\" {method} failed"), log_arg(&e));
                RewardResult::fail("error")
            }
        }
    }

    /// `grant(kind, args)`: grants `amount` (1 to 1,000,000,000). Never fails.
    pub async fn grant(&self, app: &App, kind: RewardKind, args: RewardArgs) -> RewardResult {
        self.dispatch(app, true, kind, args).await
    }

    /// `take(kind, args)`: takes `amount` (same contract as grant). Never fails.
    pub async fn take(&self, app: &App, kind: RewardKind, args: RewardArgs) -> RewardResult {
        self.dispatch(app, false, kind, args).await
    }
}
