//! Port of `src/services/userTimezoneService.js`: personal timezones (`/timezone set`), one `UserTimezone` row per
//! user, global across servers. Reads go memo (per app, 30 s) -> Redis (1 h, misses cached too) -> Postgres, so
//! /timestamp and its autocomplete rarely touch the database. Writes go to Postgres first, then refresh the caches.
//!
//! A lookup is tri-state, like the JS `string | null | undefined`: `Some(Some(zone))` is a saved zone,
//! `Some(None)` is "none saved", `None` is "unknown" (the lookup failed, or a cache-only miss).

use std::collections::VecDeque;
use std::sync::Mutex;

use bot_core::App;
use bot_db::{GuildSettings, USER_TIMEZONE_INFO};
use bot_util::time::is_valid_time_zone;
use bot_util::timezones::{ResolvedTimezone, resolve_timezone};
use serde_json::{Value, json};
use serenity::all::{Guild, Member, RoleId};

pub const MEMO_TTL_MS: i64 = 30_000;
pub const MEMO_MAX: usize = 5000;
pub const CACHE_TTL_S: u64 = 3600;

/// `userTimezoneKey(userId)`: `usertz:{userId}`.
pub fn user_timezone_key(user_id: &str) -> String {
    format!("usertz:{user_id}")
}

type MemoMap = (std::collections::HashMap<String, (Option<String>, i64)>, VecDeque<String>);

/// The memo (`Map` in insertion order, so the oldest entry is evicted first).
#[derive(Default)]
pub struct UserTimezoneMemo(Mutex<MemoMap>);

fn remember(app: &App, user_id: &str, zone: Option<String>) {
    let memo = app.ext::<UserTimezoneMemo>();
    let mut g = memo.0.lock().unwrap_or_else(|e| e.into_inner());
    let (map, order) = &mut *g;
    if map.len() >= MEMO_MAX && !map.contains_key(user_id) {
        while let Some(oldest) = order.pop_front() {
            if map.remove(&oldest).is_some() {
                break;
            }
        }
    }
    if !map.contains_key(user_id) {
        order.push_back(user_id.to_string());
    }
    map.insert(user_id.to_string(), (zone, app.clock().now_ms()));
}

/// `peekUserTimezone(userId)`: the memoised zone, with no I/O. `None` when not memoised (or expired).
pub fn peek_user_timezone(app: &App, user_id: &str) -> Option<Option<String>> {
    let memo = app.ext::<UserTimezoneMemo>();
    let mut g = memo.0.lock().unwrap_or_else(|e| e.into_inner());
    let (map, order) = &mut *g;
    let (zone, at) = map.get(user_id)?.clone();
    if app.clock().now_ms() - at > MEMO_TTL_MS {
        map.remove(user_id);
        order.retain(|u| u != user_id);
        return None;
    }
    Some(zone)
}

/// `__resetUserTimezoneMemo()`: forgets the memo.
pub fn reset_user_timezone_memo(app: &App) {
    let memo = app.ext::<UserTimezoneMemo>();
    let mut g = memo.0.lock().unwrap_or_else(|e| e.into_inner());
    g.0.clear();
    g.1.clear();
}

/// The cached value (`{ zone }`), as the JS writes it (`{"zone":null}` for none).
pub fn cache_value(zone: Option<&str>) -> Value {
    json!({ "zone": zone })
}

/// Reads a cached value: a hit when it is an object with a `zone` key (`'zone' in cached`); an invalid zone is none.
pub fn read_cache_value(v: &Value) -> Option<Option<String>> {
    let zone = v.as_object()?.get("zone")?;
    Some(zone.as_str().filter(|z| is_valid_time_zone(z)).map(String::from))
}

/// `getUserTimezone(userId, { cacheOnly })`: a user's own zone. Never fails. `cache_only`: memo and Redis only
/// (autocomplete).
pub async fn get_user_timezone(app: &App, user_id: &str, cache_only: bool) -> Option<Option<String>> {
    if let Some(p) = peek_user_timezone(app, user_id) {
        return Some(p);
    }
    if let Some(zone) = app.cache().get(&user_timezone_key(user_id)).await.as_ref().and_then(read_cache_value) {
        remember(app, user_id, zone.clone());
        return Some(zone);
    }
    if cache_only {
        return None;
    }
    let row: Result<Option<(String,)>, sqlx::Error> =
        sqlx::query_as(r#"SELECT "zone" FROM "UserTimezone" WHERE "userId" = $1"#).bind(user_id).fetch_optional(app.db()).await;
    match row {
        Ok(row) => {
            let zone = row.map(|r| r.0).filter(|z| is_valid_time_zone(z));
            remember(app, user_id, zone.clone());
            app.cache().set(&user_timezone_key(user_id), &cache_value(zone.as_deref()), CACHE_TTL_S).await;
            Some(zone)
        }
        Err(e) => {
            bot_util::log_warn!(format!("[timezone] lookup failed for {user_id}: {e}"));
            None
        }
    }
}

/// `setUserTimezone(userId, zone)`: saves a user's own zone (already validated). Fails when Postgres fails.
pub async fn set_user_timezone(app: &App, user_id: &str, zone: &str) -> Result<(), bot_db::DbError> {
    bot_db::InsertBuilder::new(&USER_TIMEZONE_INFO)
        .values([("userId", json!(user_id)), ("zone", json!(zone))])?
        .on_conflict_update(&["userId"], [("zone", json!(zone))])?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    remember(app, user_id, Some(zone.to_string()));
    app.cache().set(&user_timezone_key(user_id), &cache_value(Some(zone)), CACHE_TTL_S).await;
    Ok(())
}

/// `clearUserTimezone(userId)`: removes a user's own zone; `true` when one was saved. Fails when Postgres fails.
pub async fn clear_user_timezone(app: &App, user_id: &str) -> Result<bool, bot_db::DbError> {
    let count = bot_db::delete_where(&USER_TIMEZONE_INFO, [("userId", json!(user_id))])?.execute(app.db()).await?;
    remember(app, user_id, None);
    app.cache().set(&user_timezone_key(user_id), &cache_value(None), CACHE_TTL_S).await;
    Ok(count > 0)
}

/// `memberRoleIds(member)`: the ids of the roles a member holds.
pub fn member_role_ids(member: Option<&Member>) -> Vec<String> {
    member.map(|m| m.roles.iter().map(|r| r.to_string()).collect()).unwrap_or_default()
}

/// `resolveMemberTimezone({ member, guild, settings, personal })`: [`resolve_timezone`] for a member. `personal` is
/// what [`get_user_timezone`] returned (`None` for unknown or none); role positions come from the cached guild.
pub fn resolve_member_timezone(member_roles: &[String], guild: Option<&Guild>, settings: Option<&GuildSettings>, personal: Option<&str>) -> ResolvedTimezone {
    let null = Value::Null;
    let settings_pair = settings.map(|s| (Some(s.timezone.as_str()), s.timezone_roles.as_ref().unwrap_or(&null)));
    resolve_timezone(
        personal,
        member_roles,
        |id| {
            guild
                .and_then(|g| id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|v| g.roles.get(&RoleId::new(v))))
                .map(|r| i64::from(r.position))
                .unwrap_or(-1)
        },
        settings_pair,
    )
}
