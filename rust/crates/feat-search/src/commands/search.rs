//! Port of `src/commands/search/search.js`: `/search anime | manga | pokemon | urban`. Reads the options and hands
//! over to [`crate::search_service`], which owns the cache, the rate limiting, the adult-title filtering, the
//! `urbanNsfwOnly` age gate and the replies. Pokémon autocomplete only filters an in-memory species index.

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::respond_autocomplete;
use bot_core::prelude::*;

use crate::search_service::{self as svc, MediaType};

/// The command's data.
pub fn data() -> SlashCommand {
    let query = |name: &str, description: &str, max: u16| Opt::string(name, description).required(true).min_length(1).max_length(max);
    SlashCommand::new("search", "Look up an anime, manga, Pokémon or Urban Dictionary entry.")
        .option(Opt::sub_command("anime", "Search AniList for an anime: score, episodes, status, genres.").option(query("query", "Anime title", 100)))
        .option(Opt::sub_command("manga", "Search AniList for a manga: score, chapters, status, genres.").option(query("query", "Manga title", 100)))
        .option(
            Opt::sub_command("pokemon", "Look up a Pokémon on PokeAPI: types, abilities, stats.")
                .option(query("name", "Name or Pokédex number", 40).autocomplete(true)),
        )
        .option(Opt::sub_command("urban", "Look up a word or phrase on Urban Dictionary.").option(query("term", "Word or phrase to define", 100)))
}

/// The command (`module 'search'`, gated, MEMBER, servers only).
pub fn command() -> CommandDef {
    CommandDef::new("search", data(), Some(ModuleKey::Search), execute)
        .permission_level(PermissionLevel::Member)
        .autocomplete(autocomplete)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("anime") => svc::execute_anilist(&ctx, MediaType::Anime).await,
        Some("manga") => svc::execute_anilist(&ctx, MediaType::Manga).await,
        Some("pokemon") => svc::execute_pokemon(&ctx).await,
        Some("urban") => svc::execute_urban(&ctx).await,
        _ => Ok(()),
    }
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    // Only /search pokemon name has autocomplete; anything else answers with no suggestions.
    if ctx.subcommand().as_deref() != Some("pokemon") {
        respond_autocomplete(&ctx, &[]).await;
        return Ok(());
    }
    svc::autocomplete_pokemon(&ctx).await
}
