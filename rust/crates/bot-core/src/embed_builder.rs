//! Port of `src/utils/embedBuilder.js`: the embed factories every bot message uses.
//!
//! The interaction router needs `errorEmbed` / `warnEmbed`, so the whole (small) file is ported here in part A.
//! Every factory clamps its inputs to Discord's limits (UTF-16 units, like the JS) and stamps the current time.

use bot_config::colors;
use bot_db::GuildSettings;
use bot_util::text::clamp_text;
use serenity::builder::{CreateEmbed, CreateEmbedFooter};
use serenity::model::Timestamp;

/// Discord's embed title limit.
pub const TITLE_MAX: usize = 256;
/// Discord's embed description limit.
pub const DESCRIPTION_MAX: usize = 4096;
/// Discord's embed footer limit.
pub const FOOTER_MAX: usize = 2048;

fn base(color: u32) -> CreateEmbed {
    CreateEmbed::new().color(color).timestamp(Timestamp::now())
}

fn fill(mut e: CreateEmbed, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        e = e.description(clamp_text(d, DESCRIPTION_MAX));
    }
    if let Some(f) = footer.filter(|f| !f.is_empty()) {
        e = e.footer(CreateEmbedFooter::new(clamp_text(f, FOOTER_MAX)));
    }
    e
}

/// `successEmbed(title, description, footer)`: green, title prefixed with `✅  `.
pub fn success_embed(title: &str, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    fill(base(colors::SUCCESS).title(clamp_text(&format!("✅  {title}"), TITLE_MAX)), description, footer)
}

/// `errorEmbed(title, description, footer)`: red, title prefixed with `❌  `.
pub fn error_embed(title: &str, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    fill(base(colors::ERROR).title(clamp_text(&format!("❌  {title}"), TITLE_MAX)), description, footer)
}

/// `infoEmbed(title, description, footer)`: blurple, title prefixed with `ℹ️  `.
pub fn info_embed(title: &str, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    fill(base(colors::INFO).title(clamp_text(&format!("ℹ️  {title}"), TITLE_MAX)), description, footer)
}

/// `warnEmbed(title, description, footer)`: yellow, title prefixed with `⚠️  `.
pub fn warn_embed(title: &str, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    fill(base(colors::WARNING).title(clamp_text(&format!("⚠️  {title}"), TITLE_MAX)), description, footer)
}

/// `neutralEmbed(title, description, footer)`: grey, no emoji prefix; an empty title is left out.
pub fn neutral_embed(title: Option<&str>, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    let mut e = base(colors::NEUTRAL);
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        e = e.title(clamp_text(t, TITLE_MAX));
    }
    fill(e, description, footer)
}

/// The guild's configured embed colour, when it is a valid 24-bit colour (`applyGuildColor`'s test).
pub fn guild_color(settings: Option<&GuildSettings>) -> Option<u32> {
    settings.and_then(|s| s.embed_color).filter(|c| (0..=0xffffff).contains(c)).map(|c| c as u32)
}

/// `applyGuildColor(embed, settings)`: the guild's embed colour, for info and neutral embeds only.
pub fn apply_guild_color(embed: CreateEmbed, settings: Option<&GuildSettings>) -> CreateEmbed {
    match guild_color(settings) {
        Some(c) => embed.color(c),
        None => embed,
    }
}

/// `guildEmbed(settings, title, description, footer)`: like [`neutral_embed`] in the guild's colour
/// (`COLORS.INFO` when none is set).
pub fn guild_embed(settings: Option<&GuildSettings>, title: Option<&str>, description: Option<&str>, footer: Option<&str>) -> CreateEmbed {
    let mut e = apply_guild_color(base(colors::INFO), settings);
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        e = e.title(clamp_text(t, TITLE_MAX));
    }
    fill(e, description, footer)
}
