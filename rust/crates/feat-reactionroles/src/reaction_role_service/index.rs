//! The per-guild panel index used by the reaction hooks (`getPanelIndex`): an in-process memo (60 s), a Redis copy
//! (`guild:{g}:rrpanels`, 300 s) and PostgreSQL. A Redis miss or outage falls through to PostgreSQL (fail open); a
//! PostgreSQL error gives `None` and the hooks skip the event (fail closed).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_core::App;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::pure::{INDEX_CACHE_TTL_S, INDEX_MEMO_TTL_MS, IndexItem, emoji_key_of_stored, index_key};
use super::store::{Panel, load_index_rows};

const INDEX_MEMO_MAX_GUILDS: usize = 10_000;

/// A panel as the index keeps it (`PanelLite`), with the JS key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelLite {
    pub id: String,
    #[serde(default)]
    pub channel_id: String,
    pub message_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub max_roles: Option<i64>,
    #[serde(default)]
    pub attached: bool,
    pub items: Vec<IndexItem>,
}

/// `toPanelLite(row)`.
pub fn to_panel_lite(p: &Panel) -> PanelLite {
    PanelLite {
        id: p.group.id.clone(),
        channel_id: p.group.channel_id.clone(),
        message_id: p.group.message_id.clone(),
        title: p.group.title.clone(),
        kind: p.group.r#type.as_str().to_string(),
        layout: p.group.layout.as_str().to_string(),
        max_roles: p.group.max_roles.map(i64::from),
        attached: p.group.attached,
        items: p
            .items
            .iter()
            .map(|i| IndexItem { role_id: i.role_id.clone(), emoji: i.emoji.clone(), key: emoji_key_of_stored(i.emoji.as_deref()) })
            .collect(),
    }
}

/// `isPanelLite(p)`: an object with a string `id`, a string `messageId` and an `items` array.
pub fn is_panel_lite(v: &Value) -> bool {
    v.get("id").is_some_and(Value::is_string) && v.get("messageId").is_some_and(Value::is_string) && v.get("items").is_some_and(Value::is_array)
}

/// What the cache holds, read the JS way: every entry must pass [`is_panel_lite`] and fit [`PanelLite`]; anything else
/// is a miss.
pub fn parse_cached_index(v: &Value) -> Option<Vec<PanelLite>> {
    let arr = v.as_array()?;
    if !arr.iter().all(is_panel_lite) {
        return None;
    }
    arr.iter().map(|p| serde_json::from_value::<PanelLite>(p.clone()).ok()).collect()
}

/// Panels by message id.
pub type PanelIndex = Arc<HashMap<String, PanelLite>>;

type Loading = Shared<BoxFuture<'static, Option<PanelIndex>>>;

#[derive(Default)]
struct IndexInner {
    memo: IndexMap<String, (i64, PanelIndex)>,
    generations: IndexMap<String, u64>,
    inflight: HashMap<String, (u64, u64, Loading)>,
    next_load: u64,
}

/// The index state of one app (the JS module-level Maps).
pub struct IndexState {
    inner: Mutex<IndexInner>,
    error_throttle: bot_util::async_util::Throttle,
}

impl Default for IndexState {
    fn default() -> Self {
        Self { inner: Mutex::new(IndexInner::default()), error_throttle: bot_util::async_util::create_throttle(60_000) }
    }
}

fn state(app: &App) -> Arc<IndexState> {
    app.ext::<IndexState>()
}

fn lock(s: &IndexState) -> std::sync::MutexGuard<'_, IndexInner> {
    s.inner.lock().unwrap_or_else(|e| e.into_inner())
}

fn gen_of(inner: &IndexInner, guild_id: &str) -> u64 {
    inner.generations.get(guild_id).copied().unwrap_or(0)
}

fn set_memo(app: &App, guild_id: &str, panels: Vec<PanelLite>) -> PanelIndex {
    let by_message: PanelIndex = Arc::new(panels.into_iter().map(|p| (p.message_id.clone(), p)).collect());
    let s = state(app);
    let mut inner = lock(&s);
    inner.memo.shift_remove(guild_id);
    inner.memo.insert(guild_id.to_string(), (app.clock().now_ms(), by_message.clone()));
    while inner.memo.len() > INDEX_MEMO_MAX_GUILDS {
        inner.memo.shift_remove_index(0);
    }
    by_message
}

/// The memo only (`peekPanelIndex`): `fresh` is false once it is older than [`INDEX_MEMO_TTL_MS`].
pub struct PeekedIndex {
    pub fresh: bool,
    pub by_message_id: PanelIndex,
}

/// `peekPanelIndex(guildId)`: reads only the in-process memo.
pub fn peek_panel_index(app: &App, guild_id: &str) -> Option<PeekedIndex> {
    let s = state(app);
    let inner = lock(&s);
    let (at, map) = inner.memo.get(guild_id)?;
    Some(PeekedIndex { fresh: app.clock().now_ms() - at < INDEX_MEMO_TTL_MS, by_message_id: map.clone() })
}

/// `__setPanelIndex(guildId, panels)`: primes the memo (tests and the dashboard).
pub fn set_panel_index(app: &App, guild_id: &str, panels: Vec<PanelLite>) -> PanelIndex {
    set_memo(app, guild_id, panels)
}

/// `__resetPanelIndex()`: forgets every memo, in-flight load and generation.
pub fn reset_panel_index(app: &App) {
    let s = state(app);
    let mut inner = lock(&s);
    inner.memo.clear();
    inner.inflight.clear();
    inner.generations.clear();
}

async fn load_index(app: App, guild_id: String, generation: u64) -> Option<PanelIndex> {
    // Redis miss or outage -> fail OPEN to Postgres (a cache miss is never a deletion).
    let key = index_key(&guild_id);
    if let Some(hit) = app.cache().get_as::<Value>(&key).await
        && let Some(panels) = parse_cached_index(&hit)
    {
        let current = gen_of(&lock(&state(&app)), &guild_id);
        if current == generation {
            return Some(set_memo(&app, &guild_id, panels));
        }
    }
    let rows = match load_index_rows(app.db(), &guild_id).await {
        Ok(r) => r,
        Err(e) => {
            // Postgres down -> fail CLOSED: callers skip the event (no role change on an unknown panel).
            if state(&app).error_throttle.allow(&guild_id) {
                bot_util::log_error!(format!("reaction-roles: could not load the panels of guild {guild_id}"), e.to_string());
            }
            return None;
        }
    };
    let panels: Vec<PanelLite> = rows.iter().map(to_panel_lite).collect();
    let current = gen_of(&lock(&state(&app)), &guild_id);
    if current != generation {
        // Invalidated while loading: use the rows for this call only.
        return Some(Arc::new(panels.into_iter().map(|p| (p.message_id.clone(), p)).collect()));
    }
    let by_message = set_memo(&app, &guild_id, panels.clone());
    app.cache().set(&key, &panels, INDEX_CACHE_TTL_S).await;
    Some(by_message)
}

/// `getPanelIndex(guildId)`: the guild's panels by message id, or `None` when they cannot be read (callers then skip
/// the event). Concurrent loads for one guild share one future.
pub async fn get_panel_index(app: &App, guild_id: &str) -> Option<PanelIndex> {
    let s = state(app);
    enum Next {
        Hit(PanelIndex),
        Join(Loading),
        Load(Loading),
    }
    let next = {
        let mut inner = lock(&s);
        let memo_hit = inner.memo.get(guild_id).filter(|(at, _)| app.clock().now_ms() - at < INDEX_MEMO_TTL_MS).map(|(_, m)| m.clone());
        let generation = gen_of(&inner, guild_id);
        let running = inner.inflight.get(guild_id).filter(|(g, _, _)| *g == generation).map(|(_, _, f)| f.clone());
        if let Some(m) = memo_hit {
            Next::Hit(m)
        } else if let Some(f) = running {
            Next::Join(f)
        } else {
            inner.next_load += 1;
            let load_id = inner.next_load;
            // The load clears its own in-flight entry when it settles (the JS `promise.then(clear, clear)`), whoever
            // polls it to the end: a caller that is dropped half-way (a hook timeout) must not leave a settled load
            // behind for later callers to join instead of reloading.
            let (a, g) = (app.clone(), guild_id.to_string());
            let fut: Loading = async move {
                let out = load_index(a.clone(), g.clone(), generation).await;
                let s = state(&a);
                let mut inner = lock(&s);
                if inner.inflight.get(&g).is_some_and(|(_, id, _)| *id == load_id) {
                    inner.inflight.remove(&g);
                }
                out
            }
            .boxed()
            .shared();
            inner.inflight.insert(guild_id.to_string(), (generation, load_id, fut.clone()));
            Next::Load(fut)
        }
    };
    match next {
        Next::Hit(m) => Some(m),
        Next::Join(f) | Next::Load(f) => f.await,
    }
}

/// `invalidatePanelIndex(guildId)`: drops the memo and the Redis copy. Call after every mutation, before Discord calls
/// that can fire our own hooks.
pub async fn invalidate_panel_index(app: &App, guild_id: &str) {
    {
        let s = state(app);
        let mut inner = lock(&s);
        let next = gen_of(&inner, guild_id) + 1;
        inner.generations.shift_remove(guild_id); // refresh the insertion order
        inner.generations.insert(guild_id.to_string(), next);
        while inner.generations.len() > INDEX_MEMO_MAX_GUILDS {
            inner.generations.shift_remove_index(0);
        }
        inner.memo.shift_remove(guild_id);
        inner.inflight.remove(guild_id);
    }
    app.cache().del_one(&index_key(guild_id)).await;
}
