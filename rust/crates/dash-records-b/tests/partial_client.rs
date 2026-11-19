//! The JS stores check `typeof ctx.discord.<method> === 'function'` per method: a client that lacks the one method a
//! step needs is answered like no client at all (loadResources, the reminder test's sendMessage check), and the
//! optional lookups (starboards assertNsfwChannel, tempVoiceHubs guildNameOf, statChannels guildFacts) are skipped.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bot_core::testing::TestApp;
use dash_collections::testing::FakeDiscord;
use dash_collections::{
    DashboardDiscord, DiscordApiError, DiscordMethod, DiscordResources, InviteRef, MemberProfile, MemberRoles, MessageRef, RecordErrorCode,
    RecordStore, StoreCtx, StoreError,
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

const GUILD: &str = "955019000000000001";
const USER: &str = "955019000000000010";

fn ctx(missing: DiscordMethod) -> StoreCtx {
    let inner = FakeDiscord::new();
    inner.set_resources(GUILD, DiscordResources::default());
    StoreCtx { guild_id: GUILD.into(), user_id: USER.into(), discord: Some(Arc::new(Partial { inner, missing })), ..Default::default() }
}

fn store(t: &TestApp, key: &str) -> Arc<dyn RecordStore> {
    dash_records_b::stores(t).into_iter().find(|(k, _)| *k == key).map(|(_, s)| s).expect("store")
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
async fn tag_role_checks_need_get_guild_resources() {
    let t = TestApp::new();
    let s = store(&t, "tags");
    let d = data(json!({ "name": "hello", "actions": [{ "type": "addRole", "roleId": "955019000000000030" }] }));
    let e = s.create(GUILD, d, &ctx(DiscordMethod::GetGuildResources)).await.expect_err("create");
    assert_eq!(unavailable(e), "Roles cannot be checked from the dashboard right now.");
}

#[tokio::test]
async fn the_reminder_test_needs_send_message() {
    let t = TestApp::new();
    let s = store(&t, "reminders");
    let e = s.action(GUILD, "x", "test", Map::new(), &ctx(DiscordMethod::SendMessage)).await.expect_err("test");
    assert_eq!(unavailable(e), "Posting messages from the dashboard is not available right now.");
}

#[tokio::test]
async fn panel_roles_need_get_guild_resources() {
    let t = TestApp::new();
    let c = StoreCtx { tasks: None, ..ctx(DiscordMethod::GetGuildResources) };
    // A ticket panel without a publish channel needs no task runner, so the resource check is the first refusal.
    let s = store(&t, "ticketPanels");
    let d = data(json!({ "name": "support", "channelNameFormat": "ticket-{number}", "options": [{ "label": "Help" }] }));
    let e = s.create(GUILD, d, &c).await.expect_err("create");
    assert_eq!(unavailable(e), "The roles and emojis of this server cannot be checked from the dashboard right now.");
}
