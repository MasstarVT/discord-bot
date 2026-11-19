//! Behaviour of the lookups and `/search` (ported from `test/search.test.js` and `test/sec-misc.test.js`): canned HTTP
//! responses through `search_service::set_fetch`, the in-memory cache and the manual clock.

use std::sync::{Arc, Mutex};

use bot_core::command::option_type;
use bot_core::error::BoxFuture;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, offline_discord_as};
use bot_util::safe_fetch::{Body, FetchOptions, FetchResponse, SafeFetchCode, SafeFetchError};
use feat_search::search_service::{self as svc, Api, MediaType, PokemonQuery, SearchCode, SearchFetch};
use serde_json::{Value, json};
use serenity::all::Permissions;

pub const NOW: i64 = 1_800_000_000_000;

/// A canned answer.
pub enum Canned {
    /// status, headers, JSON body (`None`: not JSON)
    Res(u16, Vec<(&'static str, String)>, Option<Value>),
    Json(Value),
    Err(SafeFetchError),
}

type Handler = Box<dyn Fn(usize, &str) -> Canned + Send + Sync>;

/// `aniFetch` / `jsonFetch`: records every call and answers from a handler.
pub struct CannedFetch {
    handler: Handler,
    pub calls: Mutex<Vec<(String, FetchOptions)>>,
    pub delay_ms: u64,
}

impl CannedFetch {
    pub fn new(handler: impl Fn(usize, &str) -> Canned + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { handler: Box::new(handler), calls: Mutex::new(Vec::new()), delay_ms: 0 })
    }

    pub fn slow(delay_ms: u64, handler: impl Fn(usize, &str) -> Canned + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { handler: Box::new(handler), calls: Mutex::new(Vec::new()), delay_ms })
    }

    /// `jsonFetch(routes)`: a URL path (without the host) to a value; anything else is a 404.
    pub fn routes(routes: Vec<(&'static str, Value)>) -> Arc<Self> {
        Self::new(move |_, url| {
            let path = url.replace(svc::POKEAPI_BASE, "").replace("https://api.urbandictionary.com", "");
            match routes.iter().find(|(p, _)| *p == path) {
                Some((_, v)) => Canned::Json(v.clone()),
                None => Canned::Err(SafeFetchError::with_status(SafeFetchCode::HttpError, 404)),
            }
        })
    }

    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn urls(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(u, _)| u.clone()).collect()
    }

    pub fn opts(&self, i: usize) -> FetchOptions {
        self.calls.lock().unwrap()[i].1.clone()
    }

    /// The parsed JSON body of call `i`.
    pub fn body(&self, i: usize) -> Value {
        match &self.calls.lock().unwrap()[i].1.body {
            Some(Body::Text(t)) => serde_json::from_str(t).unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    fn answer(&self, url: &str, opts: &FetchOptions) -> Canned {
        let n = {
            let mut c = self.calls.lock().unwrap();
            c.push((url.to_string(), opts.clone()));
            c.len()
        };
        (self.handler)(n, url)
    }
}

pub struct Shared(pub Arc<CannedFetch>);

impl SearchFetch for Shared {
    fn fetch(&self, url: String, opts: FetchOptions) -> BoxFuture<Result<FetchResponse, SafeFetchError>> {
        let f = self.0.clone();
        Box::pin(async move {
            if f.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(f.delay_ms)).await;
            }
            match f.answer(&url, &opts) {
                Canned::Res(status, headers, body) => Ok(FetchResponse {
                    ok: (200..300).contains(&status),
                    status,
                    status_text: String::new(),
                    headers: headers.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect(),
                    url,
                    buffer: body.map(|b| b.to_string()).unwrap_or_else(|| "not json".into()).into_bytes(),
                }),
                Canned::Json(v) => Ok(FetchResponse { ok: true, status: 200, status_text: String::new(), headers: Default::default(), url, buffer: v.to_string().into_bytes() }),
                Canned::Err(e) => Err(e),
            }
        })
    }

    fn fetch_json(&self, url: String, opts: FetchOptions) -> BoxFuture<Result<Value, SafeFetchError>> {
        let f = self.0.clone();
        Box::pin(async move {
            if f.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(f.delay_ms)).await;
            }
            match f.answer(&url, &opts) {
                Canned::Json(v) => Ok(v),
                Canned::Err(e) => Err(e),
                Canned::Res(status, _, body) => match body {
                    Some(b) if (200..300).contains(&status) => Ok(b),
                    _ => Err(SafeFetchError::with_status(SafeFetchCode::HttpError, status)),
                },
            }
        })
    }
}

pub fn frieren() -> Value {
    json!({
        "id": 154587, "siteUrl": "https://anilist.co/anime/154587", "isAdult": false, "type": "ANIME", "format": "TV",
        "status": "FINISHED", "episodes": 28, "duration": 24, "chapters": null, "volumes": null, "season": "FALL", "seasonYear": 2023,
        "averageScore": 91, "meanScore": 91, "genres": ["Adventure", "Drama", "Fantasy"],
        "title": { "romaji": "Sousou no Frieren", "english": "Frieren: Beyond Journey’s End", "native": "葬送のフリーレン" },
        "description": "The adventure is over but life goes on for an elf mage.\n<br><br>\n(Source: Crunchyroll)",
        "coverImage": { "extraLarge": "https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx154587-qQTzQnEJJ3oB.jpg", "large": null },
        "startDate": { "year": 2023 }, "endDate": { "year": 2024 }, "nextAiringEpisode": null, "studios": { "nodes": [{ "name": "MADHOUSE" }] }
    })
}

pub fn adult() -> Value {
    let mut a = frieren();
    a["id"] = json!(1);
    a["isAdult"] = json!(true);
    a["title"] = json!({ "romaji": "Adult Title", "english": null, "native": null });
    a
}

pub fn pikachu() -> Value {
    json!({
        "id": 25, "name": "pikachu", "height": 4, "weight": 60, "species": { "name": "pikachu" },
        "types": [{ "slot": 1, "type": { "name": "electric" } }],
        "abilities": [{ "slot": 3, "is_hidden": true, "ability": { "name": "lightning-rod" } }, { "slot": 1, "is_hidden": false, "ability": { "name": "static" } }],
        "stats": [{ "base_stat": 35, "stat": { "name": "hp" } }, { "base_stat": 90, "stat": { "name": "speed" } }],
        "sprites": { "front_default": "https://raw.githubusercontent.com/PokeAPI/sprites/master/sprites/pokemon/25.png" }
    })
}

pub fn pikachu_species() -> Value {
    json!({
        "id": 25, "name": "pikachu", "is_legendary": false, "is_mythical": false, "generation": { "name": "generation-i" },
        "names": [{ "name": "Pikachu", "language": { "name": "en" } }], "genera": [{ "genus": "Mouse Pokémon", "language": { "name": "en" } }],
        "varieties": [{ "is_default": true, "pokemon": { "name": "pikachu", "url": "https://pokeapi.co/api/v2/pokemon/25/" } }]
    })
}

pub fn yeet() -> Value {
    json!({
        "word": "Yeet", "definition": "To discard an [item] at a [high] [velocity]", "example": "[Alex] yeets the can",
        "author": "Le anonymous ", "permalink": "https://www.urbandictionary.com/define.php?term=Yeet&defid=12339851",
        "thumbs_up": 0, "thumbs_down": 0, "written_on": "2018-04-22T00:00:00.000Z"
    })
}

pub fn ani_body(media: Vec<Value>) -> Canned {
    Canned::Res(200, vec![("x-ratelimit-remaining", "29".into())], Some(json!({ "data": { "Page": { "media": media } } })))
}

/// `setup({ safeFetch })`: a fresh app at NOW with the canned network.
pub fn setup(fetch: &Arc<CannedFetch>) -> TestApp {
    let t = TestApp::new();
    t.clock.set(NOW);
    svc::set_fetch(&t, Arc::new(Shared(fetch.clone())));
    t
}

pub fn code(r: Result<impl Sized, svc::SearchError>) -> Option<SearchCode> {
    r.err().map(|e| e.code)
}

pub fn ttl_secs(t: &TestApp, key: &str) -> i64 {
    (t.cache_backend.pttl(key) as f64 / 1000.0).round() as i64
}

// ── A guild for the command flows ─────────────────────────────────────────────

pub const G: &str = "930000000000000001";
pub const SFW: &str = "830000000000000001";
pub const NSFW: &str = "830000000000000002";
pub const THREAD_SFW: &str = "830000000000000003";
pub const THREAD_NSFW: &str = "830000000000000004";
pub const USER: &str = "630000000000000001";

/// A guild with an SFW and an age-restricted channel, a thread under each, and the bot.
pub fn search_guild(t: &TestApp) {
    let fg = FakeGuild::new(G, "630000000000000099")
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .member(TEST_BOT_ID, &[])
        .member(USER, &[])
        .channel(SFW, 0, "general", &[])
        .channel(NSFW, 0, "nsfw", &[])
        .thread(THREAD_SFW, 11, SFW)
        .thread(THREAD_NSFW, 11, NSFW);
    let mut json = fg.json();
    if let Some(chs) = json["channels"].as_array_mut() {
        for c in chs {
            if c["id"] == NSFW {
                c["nsfw"] = json!(true);
            }
        }
    }
    t.set_discord(offline_discord_as(&[json]));
}

pub async fn command_app(fetch: &Arc<CannedFetch>, patch: SettingsPatch) -> TestApp {
    let t = setup(fetch).with_guilds([G]);
    search_guild(&t);
    t.install(super::registry());
    t.settings().update_settings(G, patch.set("searchEnabled", true)).await.expect("settings");
    t
}

pub fn slash(sub: &str, option: &str, value: &str, channel: &str, user: &str) -> serenity::all::Interaction {
    FakeInteraction::slash("search").guild(G).user(user).channel(channel).subcommand(sub).option(option, option_type::STRING, json!(value)).build()
}

pub async fn run(t: &TestApp, i: serenity::all::Interaction) -> Vec<&'static str> {
    t.responses.clear();
    route_interaction(t, i).await;
    t.responses.names()
}

pub fn last(t: &TestApp, method: &str) -> bot_core::interaction::RecordedCall {
    t.responses.calls().into_iter().rev().find(|c| c.method == method).unwrap_or_else(|| panic!("no {method} in {:?}", t.responses.names()))
}

pub fn kind_anime() -> MediaType {
    MediaType::Anime
}

pub fn pq(slug: &str) -> PokemonQuery {
    PokemonQuery::Name(slug.into())
}

pub fn api_pause(api: Api) -> String {
    api.pause_key()
}
