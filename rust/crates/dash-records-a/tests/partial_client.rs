//! The JS stores check `typeof ctx.discord.<method> === 'function'` per method: a client that lacks the one method a
//! step needs is answered like no client at all (requireDiscord / needDiscord / checkHolder / loadResources), or the
//! decoration is skipped (namesFor). `DashboardDiscord::supports` says which methods a client offers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bot_core::testing::TestApp;
use dash_collections::testing::FakeDiscord;
use dash_collections::{
    DashboardDiscord, DiscordApiError, DiscordMethod, DiscordResources, InviteRef, MemberProfile, MemberRoles, MessageRef, RecordErrorCode,
    StoreCtx, StoreError,
};
use serde_json::{Map, Value, json};

/// A FakeDiscord that does not offer one method (a partial JS client).
struct Partial {
    inner: FakeDiscord,
    missing: DiscordMethod,
}

#[async_trait]
impl DashboardDiscord for Partial {
    async fn get_guild_resources(&self, guild_id: &str, fresh: bool) -> Result<Arc<DiscordResources>, DiscordApiError> {
        self.inner.get_guild_resources(guild_id, fresh).await
    }
    async fn get_member(&self, guild_id: &str, user_id: &str) -> Result<Option<MemberRoles>, DiscordApiError> {
        self.inner.get_member(guild_id, user_id).await
    }
    async fn search_members(&self, guild_id: &str, query: &str) -> Result<Vec<MemberProfile>, DiscordApiError> {
        self.inner.search_members(guild_id, query).await
    }
    async fn get_users(&self, ids: &[String]) -> Result<HashMap<String, Option<MemberProfile>>, DiscordApiError> {
        self.inner.get_users(ids).await
    }
    async fn send_message(&self, channel_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError> {
        self.inner.send_message(channel_id, payload).await
    }
    async fn edit_message(&self, channel_id: &str, message_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError> {
        self.inner.edit_message(channel_id, message_id, payload).await
    }
    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordApiError> {
        self.inner.delete_message(channel_id, message_id).await
    }
    async fn create_invite(&self, channel_id: &str, reason: &str) -> Result<InviteRef, DiscordApiError> {
        self.inner.create_invite(channel_id, reason).await
    }
    fn supports(&self, method: DiscordMethod) -> bool {
        method != self.missing
    }
}

const GUILD: &str = "955018000000000001";
const USER: &str = "955018000000000010";

fn partial(missing: DiscordMethod) -> Arc<Partial> {
    let inner = FakeDiscord::new();
    inner.set_resources(GUILD, DiscordResources::default());
    inner.set_user(MemberProfile { id: USER.into(), username: "someone".into(), ..Default::default() });
    Arc::new(Partial { inner, missing })
}

fn ctx(discord: Arc<Partial>) -> StoreCtx {
    StoreCtx { guild_id: GUILD.into(), user_id: USER.into(), discord: Some(discord), ..Default::default() }
}

fn unavailable(e: StoreError) -> String {
    match e {
        StoreError::Record(r) if r.code == RecordErrorCode::Unavailable => r.message,
        other => panic!("expected unavailable, got {other:?}"),
    }
}

fn data(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

#[tokio::test]
async fn embeds_need_the_method_of_each_step() {
    let t = TestApp::new();
    let s = dash_records_a::store_for("embeds", &t).expect("store");
    let text = "Posting messages from the dashboard is not available right now.";
    let e = s.action(GUILD, "x", "publish", data(json!({ "channelId": "955018000000000020" })), &ctx(partial(DiscordMethod::SendMessage))).await;
    assert_eq!(unavailable(e.expect_err("publish")), text);
    let e = s.action(GUILD, "x", "unpublish", Map::new(), &ctx(partial(DiscordMethod::DeleteMessage))).await;
    assert_eq!(unavailable(e.expect_err("unpublish")), text);
}

#[tokio::test]
async fn feeds_ping_role_check_needs_get_guild_resources() {
    let t = TestApp::new();
    let s = dash_records_a::store_for("feeds", &t).expect("store");
    let d = data(json!({
        "type": "RSS", "source": "https://example.com/feed.xml", "channelId": "955018000000000020", "pingRoleId": "955018000000000030",
    }));
    let e = s.create(GUILD, d, &ctx(partial(DiscordMethod::GetGuildResources))).await.expect_err("create");
    assert_eq!(unavailable(e), "The server cannot be read from the dashboard right now.");
}

#[tokio::test]
async fn balances_holder_check_needs_get_users() {
    let t = TestApp::new();
    let s = dash_records_a::store_for("balances", &t).expect("store");
    let e = s.create(GUILD, data(json!({ "userId": USER, "amount": 5 })), &ctx(partial(DiscordMethod::GetUsers))).await.expect_err("create");
    assert_eq!(unavailable(e), "The dashboard cannot check that user with Discord right now. Try again in a moment.");
}

#[tokio::test]
async fn automation_presets_need_get_guild_resources() {
    let t = TestApp::new();
    let s = dash_records_a::store_for("automations", &t).expect("store");
    let d = data(json!({ "name": "welcome", "preset": "welcomer", "presetChannelId": "955018000000000020", "trigger": "MEMBER_JOIN" }));
    let e = s.create(GUILD, d, &ctx(partial(DiscordMethod::GetGuildResources))).await.expect_err("create");
    assert_eq!(unavailable(e), "This cannot be checked with Discord from the dashboard right now.");
}

#[tokio::test]
async fn names_are_skipped_without_get_users() {
    let with: Arc<dyn DashboardDiscord> = partial(DiscordMethod::CreateInvite);
    let names = dash_records_a::giveaways_store::names_for(Some(&with), &[USER.to_string()]).await;
    assert_eq!(names.get(USER).map(String::as_str), Some("someone"));
    let p = partial(DiscordMethod::GetUsers);
    let without: Arc<dyn DashboardDiscord> = p.clone();
    let names = dash_records_a::giveaways_store::names_for(Some(&without), &[USER.to_string()]).await;
    assert!(names.is_empty());
    assert!(p.inner.calls().is_empty(), "getUsers is not called on a client that lacks it");
}
