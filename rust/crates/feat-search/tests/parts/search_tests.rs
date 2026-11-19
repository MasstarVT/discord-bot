//! `/search` and the lookups (ported from `test/search.test.js` and the budget tests of `test/sec-misc.test.js`).

use std::sync::Arc;

use bot_core::prelude::*;
use bot_util::safe_fetch::{Body, SafeFetchCode, SafeFetchError};
use feat_search::search_service::{self as svc, Api, MediaType, PokemonQuery, SearchCode};
use serde_json::{Value, json};

use crate::common::search_flows::*;

// ── AniList ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn anilist_request_options_adult_variable_and_the_six_hour_cache() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = setup(&fetch);
    let first = svc::search_anilist(&t, MediaType::Anime, "  Frieren ", false, None, None).await.expect("ok");
    assert!(!first.cached);
    assert_eq!(first.results.len(), 1);
    let (url, opts) = fetch.calls.lock().unwrap()[0].clone();
    assert_eq!(url, svc::ANILIST_URL);
    assert_eq!(opts.method, "POST");
    assert_eq!(opts.timeout_ms, 8000);
    assert_eq!(opts.max_bytes, 512 * 1024);
    assert!(matches!(opts.body, Some(Body::Text(_))));
    assert_eq!(fetch.body(0)["query"], svc::ANILIST_QUERY);
    assert_eq!(fetch.body(0)["variables"], json!({ "search": "Frieren", "type": "ANIME", "perPage": 5, "isAdult": false }));
    let key = svc::cache_key(Api::AniList, "anime|sfw|frieren");
    assert_eq!(ttl_secs(&t, &key), 21_600);
    let stored: Value = serde_json::from_str(&t.cache_backend.raw(&key).expect("cached")).expect("json");
    assert_eq!(stored["v"], 1);

    let second = svc::search_anilist(&t, MediaType::Anime, "FRIEREN", false, None, None).await.expect("ok");
    assert!(second.cached);
    assert_eq!(fetch.count(), 1);

    // The adult mode is part of the key, and NSFW channels omit isAdult entirely (never null).
    let nsfw = svc::search_anilist(&t, MediaType::Anime, "frieren", true, None, None).await.expect("ok");
    assert!(!nsfw.cached);
    assert_eq!(fetch.count(), 2);
    assert!(fetch.body(1)["variables"].get("isAdult").is_none());
    assert!(t.cache_backend.raw(&svc::cache_key(Api::AniList, "anime|nsfw|frieren")).is_some());

    svc::search_anilist(&t, MediaType::Manga, "frieren", false, None, None).await.expect("ok");
    assert_eq!(fetch.body(2)["variables"]["type"], "MANGA");
    let empty = svc::search_anilist(&t, MediaType::Anime, "   ", false, None, None).await.expect("ok");
    assert!(empty.results.is_empty() && !empty.cached);
    assert_eq!(fetch.count(), 3);
}

#[tokio::test]
async fn adult_titles_are_dropped_client_side_and_empty_results_cache_for_an_hour() {
    let fetch = CannedFetch::new(|n, _| ani_body(if n == 1 { vec![adult(), frieren()] } else { vec![] }));
    let t = setup(&fetch);
    let sfw = svc::search_anilist(&t, MediaType::Anime, "mixed", false, None, None).await.expect("ok");
    assert_eq!(sfw.results.iter().map(|r| r.id).collect::<Vec<_>>(), vec![154587]);
    let none = svc::search_anilist(&t, MediaType::Anime, "nothing here", false, None, None).await.expect("ok");
    assert!(none.results.is_empty());
    assert_eq!(ttl_secs(&t, &svc::cache_key(Api::AniList, "anime|sfw|nothing here")), 3600);
    assert!(svc::search_anilist(&t, MediaType::Anime, "nothing here", false, None, None).await.expect("ok").cached);
}

#[tokio::test]
async fn anilist_429_pauses_misses_but_still_serves_cache_hits() {
    let fetch = CannedFetch::new(|n, _| if n == 1 { ani_body(vec![frieren()]) } else { Canned::Res(429, vec![("retry-after", "42".into())], None) });
    let t = setup(&fetch);
    svc::search_anilist(&t, MediaType::Anime, "primed", false, None, None).await.expect("ok");
    let err = svc::search_anilist(&t, MediaType::Anime, "other", false, None, None).await.expect_err("429");
    assert_eq!(err.code, SearchCode::RateLimited);
    assert_eq!(err.retry_at, Some(NOW + 42_000));
    assert_eq!(ttl_secs(&t, svc::ANILIST_PAUSE_KEY), 42);
    assert_eq!(t.cache_backend.raw(svc::ANILIST_PAUSE_KEY).as_deref(), Some((NOW + 42_000).to_string().as_str()));
    let blocked = svc::search_anilist(&t, MediaType::Anime, "third", false, None, None).await.expect_err("paused");
    assert_eq!(blocked.code, SearchCode::RateLimited);
    assert_eq!(fetch.count(), 2, "a paused source is never contacted");
    assert!(svc::search_anilist(&t, MediaType::Anime, "primed", false, None, None).await.expect("hit").cached);
}

#[tokio::test]
async fn anilist_remaining_zero_returns_the_results_and_pauses_the_next_miss() {
    let fetch = CannedFetch::new(|_, _| Canned::Res(200, vec![("X-RateLimit-Remaining", "0".into())], Some(json!({ "data": { "Page": { "media": [frieren()] } } }))));
    let t = setup(&fetch);
    assert_eq!(svc::search_anilist(&t, MediaType::Anime, "last one", false, None, None).await.expect("ok").results.len(), 1);
    assert_eq!(code(svc::search_anilist(&t, MediaType::Anime, "next one", false, None, None).await), Some(SearchCode::RateLimited));
    assert_eq!(fetch.count(), 1);
}

#[tokio::test]
async fn a_pause_stored_by_another_shard_is_honoured_and_an_absurd_one_ignored() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = setup(&fetch);
    t.cache_backend.put_raw(svc::ANILIST_PAUSE_KEY, &(NOW + 30_000).to_string(), Some(60_000));
    assert_eq!(code(svc::search_anilist(&t, MediaType::Anime, "x", false, None, None).await), Some(SearchCode::RateLimited));
    assert_eq!(fetch.count(), 0);
    let t2 = setup(&fetch);
    t2.cache_backend.put_raw(svc::ANILIST_PAUSE_KEY, &(NOW + 10_000_000).to_string(), Some(60_000));
    assert_eq!(svc::search_anilist(&t2, MediaType::Anime, "x", false, None, None).await.expect("ok").results.len(), 1);
}

#[tokio::test]
async fn anilist_failures_are_mapped_and_never_cached() {
    let fetch = CannedFetch::new(|n, _| match n {
        1 => Canned::Err(SafeFetchError::new(SafeFetchCode::Timeout, Some("slow"))),
        2 => Canned::Res(500, vec![], None),
        3 => Canned::Res(200, vec![], Some(json!({ "errors": [{ "message": "boom" }], "data": null }))),
        4 => Canned::Err(SafeFetchError::new(SafeFetchCode::Network, Some("down"))),
        _ => ani_body(vec![frieren()]),
    });
    let t = setup(&fetch);
    let mut codes = Vec::new();
    for _ in 0..4 {
        let e = svc::search_anilist(&t, MediaType::Anime, "flaky", false, None, None).await.expect_err("fails");
        codes.push((e.code, e.status));
    }
    assert_eq!(
        codes,
        vec![(SearchCode::Timeout, None), (SearchCode::Unavailable, Some(500)), (SearchCode::Unavailable, None), (SearchCode::Unavailable, None)]
    );
    assert!(t.cache_backend.raw(&svc::cache_key(Api::AniList, "anime|sfw|flaky")).is_none(), "failures write nothing");
    assert_eq!(svc::search_anilist(&t, MediaType::Anime, "flaky", false, None, None).await.expect("ok").results.len(), 1);
}

#[tokio::test]
async fn per_user_cooldown_refuses_a_second_fetch_but_not_cache_hits() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = setup(&fetch);
    svc::search_anilist(&t, MediaType::Anime, "one", false, Some("u1"), None).await.expect("ok");
    assert!(t.cache_backend.raw("search:cd:u1").is_some());
    assert_eq!(ttl_secs(&t, "search:cd:u1"), 3);
    assert_eq!(code(svc::search_anilist(&t, MediaType::Anime, "two", false, Some("u1"), None).await), Some(SearchCode::Cooldown));
    assert_eq!(fetch.count(), 1);
    assert!(svc::search_anilist(&t, MediaType::Anime, "one", false, Some("u1"), None).await.expect("hit").cached);
    assert!(!svc::search_anilist(&t, MediaType::Anime, "two", false, Some("u2"), None).await.expect("ok").cached);
}

#[tokio::test]
async fn single_flight_concurrent_identical_lookups_share_one_request() {
    let fetch = CannedFetch::slow(25, |_, _| ani_body(vec![frieren()]));
    let t = setup(&fetch);
    let (a, b, c) = tokio::join!(
        svc::search_anilist(&t, MediaType::Anime, "same query", false, Some("a"), None),
        svc::search_anilist(&t, MediaType::Anime, "same query", false, Some("b"), None),
        svc::search_anilist(&t, MediaType::Anime, "same query", false, Some("c"), None),
    );
    assert_eq!(fetch.count(), 1);
    for r in [a, b, c] {
        assert_eq!(r.expect("ok").results.len(), 1);
    }
}

#[tokio::test]
async fn the_bot_wide_budget_refuses_the_25th_lookup_with_redis_up_and_down() {
    for down in [false, true] {
        let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
        let t = setup(&fetch);
        t.cache_backend.set_available(!down);
        let limit = Api::AniList.budget_per_minute();
        for i in 0..limit {
            svc::search_anilist(&t, MediaType::Anime, &format!("q{i}"), false, None, None).await.expect("within budget");
        }
        let err = svc::search_anilist(&t, MediaType::Anime, "one too many", false, None, None).await.expect_err("over budget");
        assert_eq!(err.code, SearchCode::RateLimited, "down={down}");
        assert_eq!(fetch.count() as i64, limit);
        assert_eq!(err.retry_at, Some((NOW / 60_000 + 1) * 60_000));
    }
}

#[tokio::test]
async fn redis_down_lookups_still_work() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = setup(&fetch);
    t.cache_backend.set_available(false);
    assert_eq!(svc::search_anilist(&t, MediaType::Anime, "x", false, Some("u1"), None).await.expect("ok").results.len(), 1);
    assert_eq!(svc::search_anilist(&t, MediaType::Anime, "x", false, Some("u1"), None).await.expect("ok").results.len(), 1);
    assert_eq!(fetch.count(), 2, "no cooldown, no cache");
}

/// sec-misc.test.js: two members of one server cannot push another server into RATE_LIMITED. The 3 s cooldown key is
/// dropped before each call (the JS test stubs setNX), so the minute slices are what is under test.
#[tokio::test]
async fn budget_slices_keep_one_server_from_using_up_the_shared_budget() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![]));
    let t = setup(&fetch);
    let bucket = NOW / 60_000;
    let guild_slice = (Api::AniList.budget_per_minute() / bot_config::limits::SEARCH_GUILD_BUDGET_DIVISOR as i64).max(1);
    let lookup = |q: String, user: &'static str, guild: &'static str| {
        let t = &t;
        async move {
            t.cache().del_one(&format!("search:cd:{user}")).await;
            code(svc::search_anilist(t, MediaType::Anime, &q, false, Some(user), Some(guild)).await)
        }
    };
    let mut results = Vec::new();
    for i in 0..Api::AniList.budget_per_minute() + 6 {
        results.push(lookup(format!("unique title {i}"), if i % 2 == 1 { "u2" } else { "u1" }, "gA").await);
    }
    assert_eq!(fetch.count() as i64, guild_slice, "the server may send only its slice upstream");
    assert_eq!(results.iter().filter(|r| r.is_none()).count() as i64, guild_slice);
    assert!(results.contains(&Some(SearchCode::Cooldown)), "the rest are refused by the slice");
    assert!(!results.contains(&Some(SearchCode::RateLimited)), "the bot-wide count is never reached");
    assert_eq!(t.cache_backend.raw(&format!("search:budget:anilist:{bucket}")), Some(guild_slice.to_string()), "a refused request used no bot-wide slot");
    assert!((119_000..=120_000).contains(&t.cache_backend.pttl(&format!("search:budget:anilist:{bucket}"))), "every counter has a TTL");

    assert_eq!(lookup("victim one".into(), "u3", "gB").await, None);
    assert_eq!(lookup("victim two".into(), "u4", "gC").await, None);
    assert_eq!(fetch.count() as i64, guild_slice + 2);

    assert_eq!(lookup("still refused".into(), "u5", "gA").await, Some(SearchCode::Cooldown));
    t.clock.set(NOW + 60_000);
    assert_eq!(lookup("next minute".into(), "u1", "gA").await, None);
    t.clock.set(NOW);
    t.cache().del_one("search:cd:u1").await;
    let err = svc::search_anilist(&t, MediaType::Anime, "refused again", false, Some("u1"), Some("gA")).await.expect_err("refused");
    assert_eq!(err.retry_at, Some(NOW + 60_000));
}

/// sec-misc.test.js: one member is held to the per-user slice across servers; the bot-wide count stays the hard cap.
#[tokio::test]
async fn one_member_is_held_to_the_user_slice_and_the_bot_wide_count_is_the_cap() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![]));
    let t = setup(&fetch);
    let mut results = Vec::new();
    for i in 0..bot_config::limits::SEARCH_USER_PER_MINUTE + 4 {
        t.cache().del_one("search:cd:u1").await;
        let guild = format!("g{}", i % 3);
        results.push(code(svc::search_anilist(&t, MediaType::Anime, &format!("solo {i}"), false, Some("u1"), Some(&guild)).await));
    }
    assert_eq!(fetch.count(), bot_config::limits::SEARCH_USER_PER_MINUTE);
    assert_eq!(results.iter().filter(|r| **r == Some(SearchCode::Cooldown)).count(), 4);

    let fetch = CannedFetch::new(|_, _| ani_body(vec![]));
    let t = setup(&fetch);
    let mut codes = Vec::new();
    for i in 0..Api::AniList.budget_per_minute() + 2 {
        let (user, guild) = (format!("user{i}"), format!("guild{i}"));
        codes.push(code(svc::search_anilist(&t, MediaType::Anime, &format!("wide {i}"), false, Some(&user), Some(&guild)).await));
    }
    assert_eq!(fetch.count() as i64, Api::AniList.budget_per_minute());
    assert_eq!(codes[codes.len() - 2..], [Some(SearchCode::RateLimited), Some(SearchCode::RateLimited)]);
}

// ── PokeAPI ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pokeapi_species_first_then_the_default_variety_cached_for_a_week() {
    let fetch = CannedFetch::routes(vec![("/pokemon-species/pikachu", pikachu_species()), ("/pokemon/pikachu", pikachu())]);
    let t = setup(&fetch);
    let r = svc::get_pokemon(&t, &svc::pokemon_slug("Pikachu").expect("slug"), None, None).await.expect("ok");
    assert_eq!(r.results[0].name, "Pikachu");
    assert_eq!(fetch.urls(), vec!["https://pokeapi.co/api/v2/pokemon-species/pikachu", "https://pokeapi.co/api/v2/pokemon/pikachu"]);
    assert_eq!(fetch.opts(0).timeout_ms, 8000);
    assert_eq!(fetch.opts(0).max_bytes, 3 * 1024 * 1024);
    let key = svc::cache_key(Api::PokeApi, "pokemon|pikachu");
    assert_eq!(ttl_secs(&t, &key), 604_800);
    assert!(svc::get_pokemon(&t, &PokemonQuery::Name("pikachu".into()), None, None).await.expect("hit").cached);
    assert_eq!(fetch.count(), 2);
    assert!(t.cache_backend.raw(&key).expect("cached").len() < 2048, "only the projected object is cached");
}

#[tokio::test]
async fn pokeapi_urls_are_built_from_names_never_followed() {
    let species = json!({ "id": 386, "name": "deoxys", "names": [{ "name": "Deoxys", "language": { "name": "en" } }],
        "varieties": [{ "is_default": true, "pokemon": { "name": "deoxys-normal", "url": "https://evil.example/steal" } }] });
    let deoxys = json!({ "id": 386, "name": "deoxys-normal", "species": { "name": "deoxys", "url": "https://evil.example/s" }, "types": [], "abilities": [], "stats": [] });
    let fetch = CannedFetch::routes(vec![("/pokemon-species/deoxys", species.clone()), ("/pokemon/deoxys-normal", deoxys)]);
    let t = setup(&fetch);
    let r = svc::get_pokemon(&t, &PokemonQuery::Name("deoxys".into()), None, None).await.expect("ok");
    assert_eq!(r.results[0].name, "Deoxys (Normal)");
    assert_eq!(r.results[0].dex, 386);
    assert!(fetch.urls().iter().all(|u| u.starts_with("https://pokeapi.co/api/v2/")));

    let mut evil = species;
    evil["varieties"] = json!([{ "is_default": true, "pokemon": { "name": "../../etc/passwd" } }]);
    let fetch2 = CannedFetch::routes(vec![("/pokemon-species/evil", evil)]);
    let t2 = setup(&fetch2);
    assert!(svc::get_pokemon(&t2, &PokemonQuery::Name("evil".into()), None, None).await.expect("ok").results.is_empty());
    assert_eq!(fetch2.count(), 1);
}

#[tokio::test]
async fn pokeapi_forms_and_ids_fall_back_and_unknown_is_cached_negatively() {
    let mega = json!({ "id": 10034, "name": "charizard-mega-x", "species": { "name": "charizard" }, "height": 17, "weight": 1105,
        "types": [{ "slot": 1, "type": { "name": "fire" } }], "abilities": [], "stats": [] });
    let charizard = json!({ "id": 6, "name": "charizard", "names": [{ "name": "Charizard", "language": { "name": "en" } }] });
    let fetch = CannedFetch::routes(vec![("/pokemon/charizard-mega-x", mega.clone()), ("/pokemon-species/charizard", charizard), ("/pokemon/10034", mega)]);
    let t = setup(&fetch);
    let r = svc::get_pokemon(&t, &PokemonQuery::Name("charizard-mega-x".into()), None, None).await.expect("ok");
    assert_eq!(r.results[0].name, "Charizard (Mega X)");
    assert_eq!(r.results[0].dex, 6);
    let paths: Vec<String> = fetch.urls().iter().map(|u| u.replace(svc::POKEAPI_BASE, "")).collect();
    assert_eq!(paths, vec!["/pokemon-species/charizard-mega-x", "/pokemon/charizard-mega-x", "/pokemon-species/charizard"]);
    assert_eq!(svc::get_pokemon(&t, &PokemonQuery::Id(10034), None, None).await.expect("ok").results[0].slug, "charizard-mega-x");
    let none = svc::get_pokemon(&t, &PokemonQuery::Name("nopemon".into()), None, None).await.expect("ok");
    assert!(none.results.is_empty());
    assert_eq!(ttl_secs(&t, &svc::cache_key(Api::PokeApi, "pokemon|nopemon")), 3600);
}

#[tokio::test]
async fn pokeapi_429_pauses_with_retry_after_and_other_errors_are_unavailable() {
    let fetch = CannedFetch::new(|_, _| {
        let mut e = SafeFetchError::with_status(SafeFetchCode::HttpError, 429);
        e.retry_after_ms = Some(90_000.0);
        Canned::Err(e)
    });
    let t = setup(&fetch);
    let err = svc::get_pokemon(&t, &PokemonQuery::Name("mew".into()), None, None).await.expect_err("429");
    assert_eq!(err.code, SearchCode::RateLimited);
    assert_eq!(err.retry_at, Some(NOW + 90_000));
    assert_eq!(ttl_secs(&t, "search:pokeapi:pause"), 90);
    assert_eq!(code(svc::get_pokemon(&t, &PokemonQuery::Name("ditto".into()), None, None).await), Some(SearchCode::RateLimited));
    assert_eq!(fetch.count(), 1);

    let t = setup(&CannedFetch::new(|_, _| Canned::Err(SafeFetchError::with_status(SafeFetchCode::HttpError, 502))));
    let bad = svc::get_pokemon(&t, &PokemonQuery::Name("mew".into()), None, None).await.expect_err("502");
    assert_eq!((bad.code, bad.status), (SearchCode::Unavailable, Some(502)));
    let not_json = CannedFetch::new(|_, _| {
        let mut e = SafeFetchError::new(SafeFetchCode::HttpError, Some("Response is not valid JSON"));
        e.status = Some(200);
        Canned::Err(e)
    });
    let t = setup(&not_json);
    assert_eq!(svc::get_pokemon(&t, &PokemonQuery::Name("mew".into()), None, None).await.expect_err("bad json").status, None);
}

#[tokio::test]
async fn pokeapi_a_network_or_dns_failure_is_retried_once() {
    let flaky = CannedFetch::new(|n, url| {
        if n == 1 {
            return Canned::Err(SafeFetchError::new(SafeFetchCode::Network, Some("Network error: ECONNRESET")));
        }
        let path = url.replace(svc::POKEAPI_BASE, "");
        match path.as_str() {
            "/pokemon-species/pikachu" => Canned::Json(pikachu_species()),
            "/pokemon/pikachu" => Canned::Json(pikachu()),
            _ => Canned::Err(SafeFetchError::with_status(SafeFetchCode::HttpError, 404)),
        }
    });
    let t = setup(&flaky);
    assert_eq!(svc::get_pokemon(&t, &PokemonQuery::Name("pikachu".into()), None, None).await.expect("ok").results[0].name, "Pikachu");
    assert_eq!(flaky.count(), 3, "the species request ran twice, then the default variety");

    let down = CannedFetch::new(|_, _| Canned::Err(SafeFetchError::new(SafeFetchCode::DnsFailed, Some("Could not resolve pokeapi.co."))));
    let t = setup(&down);
    let err = svc::get_pokemon(&t, &PokemonQuery::Name("mew".into()), None, None).await.expect_err("dns");
    assert_eq!((err.code, err.status), (SearchCode::Unavailable, None));
    assert_eq!(down.count(), 2);

    let slow = CannedFetch::new(|_, _| Canned::Err(SafeFetchError::new(SafeFetchCode::Timeout, Some("slow"))));
    let t = setup(&slow);
    assert_eq!(code(svc::get_pokemon(&t, &PokemonQuery::Name("mew".into()), None, None).await), Some(SearchCode::Timeout));
    assert_eq!(slow.count(), 1, "a timeout is not retried");
}

async fn wait_for<F: Fn() -> bool>(f: F) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn the_pokemon_index_is_read_from_redis_else_refreshed_in_the_background_with_a_back_off() {
    let fetch = CannedFetch::routes(vec![]);
    let t = setup(&fetch);
    t.cache_backend.put_raw(svc::POKEAPI_INDEX_KEY, r#"{"v":1,"entries":[["pikachu",25]]}"#, Some(60_000));
    assert_eq!(svc::get_pokemon_index(&t).await, Some(vec![("pikachu".to_string(), 25)]));
    assert_eq!(fetch.count(), 0);

    let body = json!({ "results": [
        { "name": "pikachu", "url": "https://pokeapi.co/api/v2/pokemon-species/25/" },
        { "name": "bulbasaur", "url": "https://pokeapi.co/api/v2/pokemon-species/1/" },
        { "name": "Bad Name!", "url": "https://pokeapi.co/api/v2/pokemon-species/2/" },
        { "name": "nourl", "url": "https://pokeapi.co/api/v2/pokemon/3/" }
    ] });
    let fetch2 = CannedFetch::routes(vec![("/pokemon-species?limit=2000", body)]);
    let t2 = setup(&fetch2);
    assert_eq!(svc::get_pokemon_index(&t2).await, None, "the first call never waits for the network");
    assert!(wait_for(|| t2.cache_backend.raw(svc::POKEAPI_INDEX_KEY).is_some()).await);
    assert_eq!(svc::get_pokemon_index(&t2).await, Some(vec![("bulbasaur".to_string(), 1), ("pikachu".to_string(), 25)]));
    assert_eq!(fetch2.count(), 1);
    assert_eq!(fetch2.opts(0).max_bytes, 1024 * 1024);
    assert_eq!(ttl_secs(&t2, svc::POKEAPI_INDEX_KEY), 604_800);
    assert_eq!(t2.cache_backend.raw(svc::POKEAPI_INDEX_KEY).as_deref(), Some(r#"{"v":1,"entries":[["bulbasaur",1],["pikachu",25]]}"#));

    let failing = CannedFetch::new(|_, _| Canned::Err(SafeFetchError::new(SafeFetchCode::Network, Some("down"))));
    let t3 = setup(&failing);
    assert_eq!(svc::get_pokemon_index(&t3).await, None);
    assert!(wait_for(|| failing.count() >= 2).await);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    svc::get_pokemon_index(&t3).await;
    svc::get_pokemon_index(&t3).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(failing.count(), 2, "one refresh (its request tried twice), then none for 10 minutes");
    t3.clock.advance(10 * 60_000 + 1);
    svc::get_pokemon_index(&t3).await;
    assert!(wait_for(|| failing.count() >= 4).await, "retried after the back-off");
}

// ── Urban Dictionary ──────────────────────────────────────────────────────────

#[tokio::test]
async fn urban_request_negative_cache_bad_shape_and_429() {
    let fetch = CannedFetch::routes(vec![
        ("/v0/define?term=Yeet%20it", json!({ "list": [yeet()] })),
        ("/v0/define?term=zzzz", json!({ "list": [] })),
        ("/v0/define?term=broken", json!({ "nope": 1 })),
    ]);
    let t = setup(&fetch);
    let r = svc::define_urban(&t, "Yeet  it", None, None).await.expect("ok");
    assert_eq!(r.results[0].word, "Yeet");
    assert_eq!(fetch.urls()[0], "https://api.urbandictionary.com/v0/define?term=Yeet%20it");
    assert_eq!(fetch.opts(0).max_bytes, 1024 * 1024);
    let key = svc::cache_key(Api::Urban, "define|yeet it");
    assert_eq!(ttl_secs(&t, &key), 21_600);
    assert!(svc::define_urban(&t, "yeet IT", None, None).await.expect("hit").cached);
    assert!(svc::define_urban(&t, "zzzz", None, None).await.expect("ok").results.is_empty());
    assert_eq!(ttl_secs(&t, &svc::cache_key(Api::Urban, "define|zzzz")), 3600);
    assert_eq!(code(svc::define_urban(&t, "broken", None, None).await), Some(SearchCode::Unavailable));
    assert!(t.cache_backend.raw(&svc::cache_key(Api::Urban, "define|broken")).is_none());
    assert!(svc::define_urban(&t, "  ", None, None).await.expect("ok").results.is_empty());

    let limited = CannedFetch::new(|_, _| Canned::Err(SafeFetchError::with_status(SafeFetchCode::HttpError, 429)));
    let t = setup(&limited);
    let rl = svc::define_urban(&t, "a", None, None).await.expect_err("429");
    assert_eq!(rl.code, SearchCode::RateLimited);
    assert_eq!(rl.retry_at, Some(NOW + 60_000));
    assert_eq!(code(svc::define_urban(&t, "b", None, None).await), Some(SearchCode::RateLimited));
    assert_eq!(limited.count(), 1);
}

// ── The command flows ─────────────────────────────────────────────────────────

#[tokio::test]
async fn urban_is_refused_privately_outside_age_restricted_channels_while_nsfw_only() {
    for patch in [SettingsPatch::new(), SettingsPatch::new().set("urbanNsfwOnly", true)] {
        let fetch = CannedFetch::routes(vec![("/v0/define?term=yeet", json!({ "list": [yeet()] }))]);
        let t = command_app(&fetch, patch).await;
        for ch in [SFW, THREAD_SFW, "830000000000000099"] {
            assert_eq!(run(&t, slash("urban", "term", "yeet", ch, USER)).await, vec!["reply"], "channel {ch}");
            let p = last(&t, "reply");
            assert!(p.ephemeral());
            assert_eq!(p.embed_title(), Some("❌  Age-Restricted Channel Required"));
            assert_eq!(p.embed_description(), Some("Urban Dictionary definitions can only be shown in age-restricted channels on this server."));
        }
        assert_eq!(fetch.count(), 0);
    }
}

#[tokio::test]
async fn urban_works_in_an_age_restricted_channel_or_anywhere_once_turned_off() {
    for (patch, channel) in [(SettingsPatch::new(), NSFW), (SettingsPatch::new().set("urbanNsfwOnly", false), SFW), (SettingsPatch::new(), THREAD_NSFW)] {
        let mut second = yeet();
        second["word"] = json!("Yeet 2");
        let fetch = CannedFetch::routes(vec![("/v0/define?term=Yeet", json!({ "list": [yeet(), second] }))]);
        let t = command_app(&fetch, patch).await;
        assert_eq!(run(&t, slash("urban", "term", " Yeet ", channel, USER)).await, vec!["deferReply", "editReply"], "channel {channel}");
        assert!(!last(&t, "deferReply").ephemeral());
        let p = last(&t, "editReply");
        assert_eq!(p.embed_title(), Some("Yeet"));
        assert_eq!(p.body["components"].as_array().map(Vec::len), Some(1), "two definitions page with buttons");
        assert_eq!(fetch.count(), 1);
    }
}

#[tokio::test]
async fn urban_invalid_no_result_and_errors_answer_privately_and_drop_the_placeholder() {
    let fetch = CannedFetch::routes(vec![("/v0/define?term=nothing", json!({ "list": [] }))]);
    let t = command_app(&fetch, SettingsPatch::new().set("urbanNsfwOnly", false)).await;
    assert_eq!(run(&t, slash("urban", "term", "  \u{0} ", SFW, USER)).await, vec!["reply"]);
    assert_eq!(last(&t, "reply").embed_title(), Some("❌  Invalid Search"));

    assert_eq!(run(&t, slash("urban", "term", "nothing", SFW, USER)).await, vec!["deferReply", "deleteReply", "followUp"]);
    let f = last(&t, "followUp");
    assert!(f.ephemeral());
    assert_eq!(f.body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(f.embed_title(), Some("⚠️  No Results"));

    let slow = CannedFetch::new(|_, _| Canned::Err(SafeFetchError::new(SafeFetchCode::Timeout, Some("slow"))));
    let t = command_app(&slow, SettingsPatch::new().set("urbanNsfwOnly", false)).await;
    assert_eq!(run(&t, slash("urban", "term", "slow", SFW, USER)).await, vec!["deferReply", "deleteReply", "followUp"]);
    assert_eq!(last(&t, "followUp").embed_title(), Some("❌  Search Timed Out"));
}

#[tokio::test]
async fn anime_pages_without_adult_titles_in_sfw_and_with_them_in_nsfw() {
    let mut second = frieren();
    second["id"] = json!(2);
    let fetch = CannedFetch::new(move |_, _| ani_body(vec![adult(), frieren(), second.clone()]));
    let t = command_app(&fetch, SettingsPatch::new().set("embedColor", 0x123456)).await;
    assert_eq!(run(&t, slash("anime", "query", "frieren", SFW, USER)).await, vec!["deferReply", "editReply"]);
    let p = last(&t, "editReply");
    assert_eq!(p.body["embeds"][0]["color"], 0x123456);
    assert_eq!(p.embed_title(), Some("Frieren: Beyond Journey’s End"));
    assert!(p.body["embeds"][0]["footer"]["text"].as_str().unwrap_or("").ends_with("Page 1/2"));
    assert_eq!(p.body["components"].as_array().map(Vec::len), Some(1));
    assert_eq!(fetch.body(0)["variables"]["isAdult"], false);

    let fetch = CannedFetch::new(|_, _| ani_body(vec![adult(), frieren()]));
    let t = command_app(&fetch, SettingsPatch::new()).await;
    run(&t, slash("manga", "query", "frieren", NSFW, USER)).await;
    assert!(fetch.body(0)["variables"].get("isAdult").is_none());
    assert_eq!(fetch.body(0)["variables"]["type"], "MANGA");
    assert!(last(&t, "editReply").body["embeds"][0]["footer"]["text"].as_str().unwrap_or("").contains("18+"));

    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = command_app(&fetch, SettingsPatch::new()).await;
    run(&t, slash("anime", "query", "solo", SFW, USER)).await;
    assert!(last(&t, "editReply").body["components"].as_array().is_none_or(Vec::is_empty), "one result: no buttons");
}

#[tokio::test]
async fn anime_invalid_no_results_rate_limit_and_cooldown_answer_privately() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![]));
    let t = command_app(&fetch, SettingsPatch::new()).await;
    assert_eq!(run(&t, slash("anime", "query", "   ", SFW, USER)).await, vec!["reply"]);
    assert!(last(&t, "reply").ephemeral());
    assert_eq!(run(&t, slash("anime", "query", "zzzzqqqq", SFW, USER)).await, vec!["deferReply", "deleteReply", "followUp"]);
    let d = last(&t, "followUp");
    assert_eq!(d.embed_title(), Some("⚠️  No Results"));
    assert_eq!(d.embed_description(), Some("No anime found for **zzzzqqqq**.\nAdult titles are only shown in age-restricted channels."));

    let fetch = CannedFetch::new(|_, _| Canned::Res(429, vec![("retry-after", "30".into())], None));
    let t = command_app(&fetch, SettingsPatch::new()).await;
    run(&t, slash("anime", "query", "limited", SFW, USER)).await;
    let rl = last(&t, "followUp");
    assert!(rl.ephemeral());
    assert_eq!(rl.embed_description(), Some(format!("AniList is limiting how often we can search. Try again <t:{}:R>.", NOW / 1000 + 30).as_str()));

    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = command_app(&fetch, SettingsPatch::new()).await;
    run(&t, slash("anime", "query", "other", SFW, "630000000000000077")).await;
    run(&t, slash("anime", "query", "another", SFW, "630000000000000077")).await;
    assert_eq!(last(&t, "followUp").embed_description(), Some("You are searching too fast. Wait a few seconds and try again."));
}

#[tokio::test]
async fn pokemon_invalid_found_and_missing() {
    let fetch = CannedFetch::routes(vec![("/pokemon-species/pikachu", pikachu_species()), ("/pokemon/pikachu", pikachu())]);
    let t = command_app(&fetch, SettingsPatch::new()).await;
    assert_eq!(run(&t, slash("pokemon", "name", "!!!", SFW, USER)).await, vec!["reply"]);
    assert_eq!(last(&t, "reply").embed_title(), Some("❌  Invalid Pokémon"));
    assert_eq!(run(&t, slash("pokemon", "name", "Pikachu", SFW, USER)).await, vec!["deferReply", "editReply"]);
    let p = last(&t, "editReply");
    assert_eq!(p.embed_title(), Some("Pikachu · #25"));
    assert_eq!(p.body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(run(&t, slash("pokemon", "name", "Nopemon", SFW, "630000000000000055")).await, vec!["deferReply", "deleteReply", "followUp"]);
    assert!(last(&t, "followUp").embed_description().unwrap_or("").contains("**Nopemon**"));
}

#[tokio::test]
async fn pokemon_autocomplete_from_the_index_and_nothing_elsewhere() {
    let fetch = CannedFetch::routes(vec![]);
    let t = command_app(&fetch, SettingsPatch::new()).await;
    t.cache_backend.put_raw(svc::POKEAPI_INDEX_KEY, r#"{"v":1,"entries":[["bulbasaur",1],["pikachu",25],["pichu",172]]}"#, Some(60_000));
    let ac = |sub: &str| bot_core::testing::FakeInteraction::autocomplete("search").guild(G).user(USER).subcommand(sub).focused("name", "pi").build();
    t.responses.clear();
    bot_core::router::route_interaction(&t, ac("pokemon")).await;
    assert_eq!(last(&t, "respond").choices(), vec![json!({ "name": "Pikachu (#25)", "value": "pikachu" }), json!({ "name": "Pichu (#172)", "value": "pichu" })]);
    t.responses.clear();
    bot_core::router::route_interaction(&t, ac("anime")).await;
    assert_eq!(last(&t, "respond").choices(), Vec::<Value>::new());

    // The module off: nothing (the router's gate answers [] first).
    t.settings().update_settings(G, SettingsPatch::new().set("searchEnabled", false)).await.expect("settings");
    t.responses.clear();
    bot_core::router::route_interaction(&t, ac("pokemon")).await;
    assert_eq!(last(&t, "respond").choices(), Vec::<Value>::new());
}

#[tokio::test]
async fn the_interaction_is_deferred_before_the_network_is_touched() {
    let fetch = CannedFetch::new(|_, _| ani_body(vec![frieren()]));
    let t = command_app(&fetch, SettingsPatch::new()).await;
    // The fetch handler sees the responses recorded so far: the defer is already there.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (seen2, responses) = (seen.clone(), t.responses.clone());
    let probe = CannedFetch::new(move |_, _| {
        seen2.lock().unwrap().push(responses.names());
        ani_body(vec![frieren()])
    });
    svc::set_fetch(&t, Arc::new(Shared(probe)));
    run(&t, slash("manga", "query", "frieren", SFW, USER)).await;
    assert_eq!(seen.lock().unwrap().clone(), vec![vec!["deferReply"]]);
}
