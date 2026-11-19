//! Serenity models as the plain structs bot-util's placeholder engine reads (`placeholderParser.js` reads discord.js
//! objects; these fill exactly the values it reads), and discord.js's CDN URL rules (`displayAvatarURL({ extension:
//! 'png', size: 256 })`, `iconURL(...)`, `defaultAvatarURL`), which differ from serenity's own helpers.

use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser, RenderContext};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, ImageHash, Member, User, UserId};

use crate::context::App;

const CDN: &str = "https://cdn.discordapp.com";

fn ext(hash: &str) -> &'static str {
    if hash.starts_with("a_") { "gif" } else { "png" }
}

/// The snowflake's creation time in epoch ms.
pub fn snowflake_ms(id: u64) -> f64 {
    ((id >> 22) + 1_420_070_400_000) as f64
}

/// discord.js `user.discriminator` (`'0'` for migrated usernames).
pub fn discriminator(user: &User) -> String {
    user.discriminator.map(|d| format!("{:04}", d.get())).unwrap_or_else(|| "0".into())
}

/// `user.tag`: the username, or `username#discriminator` for a legacy account.
pub fn user_tag(user: &User) -> String {
    match user.discriminator {
        None => user.name.clone(),
        Some(d) => format!("{}#{:04}", user.name, d.get()),
    }
}

/// `user.defaultAvatarURL`.
pub fn default_avatar_url(user_id: UserId, discriminator: &str) -> String {
    let index = if discriminator == "0" { (user_id.get() >> 22) % 6 } else { discriminator.parse::<u64>().unwrap_or(0) % 5 };
    format!("{CDN}/embed/avatars/{index}.png")
}

/// `user.avatarURL({ extension: 'png', size: 256 })`.
pub fn user_avatar_url(user_id: UserId, hash: &ImageHash) -> String {
    let h = hash.to_string();
    format!("{CDN}/avatars/{user_id}/{h}.{}?size=256", ext(&h))
}

/// `user.displayAvatarURL({ extension: 'png', size: 256 })`.
pub fn user_display_avatar_url(user: &User) -> String {
    match &user.avatar {
        Some(h) => user_avatar_url(user.id, h),
        None => default_avatar_url(user.id, &discriminator(user)),
    }
}

/// `member.displayAvatarURL({ extension: 'png', size: 256 })`: the server avatar first.
pub fn member_display_avatar_url(guild_id: GuildId, member: &Member) -> String {
    match &member.avatar {
        Some(h) => {
            let h = h.to_string();
            format!("{CDN}/guilds/{guild_id}/users/{}/avatars/{h}.{}?size=256", member.user.id, ext(&h))
        }
        None => user_display_avatar_url(&member.user),
    }
}

/// `guild.iconURL({ extension: 'png', size: 256 })`.
pub fn guild_icon_url(guild_id: GuildId, icon: Option<&ImageHash>) -> Option<String> {
    icon.map(|h| {
        let h = h.to_string();
        format!("{CDN}/icons/{guild_id}/{h}.{}?size=256", ext(&h))
    })
}

/// `user.displayName`: the global name, else the username.
pub fn user_display_name(user: &User) -> String {
    user.global_name.clone().unwrap_or_else(|| user.name.clone())
}

/// `member.displayName`: the nickname, else the user's display name.
pub fn member_display_name(member: &Member) -> String {
    member.nick.clone().unwrap_or_else(|| user_display_name(&member.user))
}

/// A user as the placeholders read it.
pub fn placeholder_user(user: &User) -> PlaceholderUser {
    PlaceholderUser {
        id: user.id.to_string(),
        username: Some(user.name.clone()),
        tag: Some(user_tag(user)),
        discriminator: Some(discriminator(user)),
        global_name: user.global_name.clone(),
        bot: user.bot,
        created_timestamp: Some(snowflake_ms(user.id.get())),
        avatar_url: Some(user_display_avatar_url(user)),
    }
}

/// A cached guild as the placeholders read it.
pub fn placeholder_guild(guild: &Guild) -> PlaceholderGuild {
    PlaceholderGuild {
        id: guild.id.to_string(),
        name: Some(guild.name.clone()),
        owner_id: Some(guild.owner_id.to_string()),
        member_count: Some(guild.member_count as f64),
        verification_level: Some(u8::from(guild.verification_level).to_string()),
        joined_timestamp: Some(guild.joined_at.timestamp_millis() as f64),
        created_timestamp: Some(snowflake_ms(guild.id.get())),
        premium_subscription_count: Some(guild.premium_subscription_count.unwrap_or(0).to_string()),
        premium_tier: Some(u8::from(guild.premium_tier).to_string()),
        icon_url: guild_icon_url(guild.id, guild.icon.as_ref()),
    }
}

/// A member (with its guild, when cached) as the placeholders read it.
pub fn placeholder_member(member: &Member, guild: Option<&Guild>) -> PlaceholderMember {
    PlaceholderMember {
        display_name: Some(member_display_name(member)),
        joined_timestamp: member.joined_at.map(|t| t.timestamp_millis() as f64),
        avatar_url: Some(member_display_avatar_url(member.guild_id, member)),
        user: Some(placeholder_user(&member.user)),
        guild: guild.map(placeholder_guild),
    }
}

/// A guild channel as the placeholders read it.
pub fn placeholder_channel(channel: &GuildChannel) -> PlaceholderChannel {
    PlaceholderChannel { id: channel.id.to_string(), name: Some(channel.name.clone()), kind: Some(i64::from(u8::from(channel.kind))) }
}

/// The usual render context: the member (or just the user), its guild and a channel, from the serenity cache.
pub fn render_context(app: &App, guild_id: Option<GuildId>, user: &User, member: Option<&Member>, channel_id: Option<ChannelId>) -> RenderContext {
    let guild = guild_id.and_then(|g| app.discord().and_then(|d| d.cache.guild(g).map(|g| g.clone())));
    let channel = match (guild.as_ref(), channel_id) {
        (Some(g), Some(c)) => crate::permissions::find_channel(g, c).map(placeholder_channel),
        _ => None,
    };
    RenderContext {
        member: member.map(|m| placeholder_member(m, guild.as_ref())),
        user: Some(placeholder_user(user)),
        guild: guild.as_ref().map(placeholder_guild),
        channel,
        vars: None,
        args: None,
    }
}

// ── CDN URLs with any options (discord.js `ImageURLOptions`) ─────────────────

/// discord.js `ImageURLOptions`: `extension` (default `webp`), `size`, `forceStatic` (an animated `a_` hash still
/// gets `extension` instead of `gif`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageUrlOptions {
    pub extension: &'static str,
    pub size: Option<u16>,
    pub force_static: bool,
}

impl Default for ImageUrlOptions {
    fn default() -> Self {
        Self { extension: "webp", size: None, force_static: false }
    }
}

impl ImageUrlOptions {
    /// `{ extension, size }`.
    pub fn new(extension: &'static str, size: Option<u16>) -> Self {
        Self { extension, size, force_static: false }
    }

    /// `forceStatic: true`.
    pub fn force_static(mut self) -> Self {
        self.force_static = true;
        self
    }

    fn url(&self, root: &str, hash: &str) -> String {
        let ext = if !self.force_static && hash.starts_with("a_") { "gif" } else { self.extension };
        match self.size {
            Some(s) => format!("{CDN}{root}/{hash}.{ext}?size={s}"),
            None => format!("{CDN}{root}/{hash}.{ext}"),
        }
    }
}

/// `user.displayAvatarURL(options)`: the avatar with these options, else `defaultAvatarURL` (always `.png`, no size).
pub fn user_display_avatar_url_with(user: &User, options: ImageUrlOptions) -> String {
    match &user.avatar {
        Some(h) => options.url(&format!("/avatars/{}", user.id), &h.to_string()),
        None => default_avatar_url(user.id, &discriminator(user)),
    }
}

/// `user.displayAvatarURL()` with discord.js's defaults (`webp`, `gif` when animated, no size).
pub fn display_avatar_url_default(user: &User) -> String {
    user_display_avatar_url_with(user, ImageUrlOptions::default())
}

/// `member.displayAvatarURL(options)`: the server avatar, else the user's display avatar.
pub fn member_display_avatar_url_with(guild_id: GuildId, member: &Member, options: ImageUrlOptions) -> String {
    match &member.avatar {
        Some(h) => options.url(&format!("/guilds/{guild_id}/users/{}/avatars", member.user.id), &h.to_string()),
        None => user_display_avatar_url_with(&member.user, options),
    }
}

/// `member.displayAvatarURL({ extension: 'png', forceStatic: true, size: 256 })` (welcome cards: never a GIF).
pub fn member_static_png_avatar_url(guild_id: GuildId, member: &Member) -> String {
    member_display_avatar_url_with(guild_id, member, ImageUrlOptions::new("png", Some(256)).force_static())
}

/// `guild.iconURL(options)`.
pub fn guild_icon_url_with(guild_id: GuildId, icon: Option<&ImageHash>, options: ImageUrlOptions) -> Option<String> {
    icon.map(|h| options.url(&format!("/icons/{guild_id}"), &h.to_string()))
}

/// `guild.iconURL()` with discord.js's defaults (`webp`, `gif` when animated, no size).
pub fn guild_icon_url_default(guild_id: GuildId, icon: Option<&ImageHash>) -> Option<String> {
    guild_icon_url_with(guild_id, icon, ImageUrlOptions::default())
}

// ── message.cleanContent ───────────────────────────────────────────────────────

static CLEAN_RE: bot_util::jsre::LazyRe = bot_util::lazy_re!(
    r"<(?:(?<type>@[!&]?|#)|(?:\/(?<commandName>[-_\p{L}\p{N}\p{sc=Deva}\p{sc=Thai} ]+):)|(?:a?:(?<emojiName>[\w]+):))(?<id>\d{17,19})>",
    "gu"
);

/// discord.js `cleanContent(str, channel)` (`message.cleanContent`): user mentions become `@displayName` (the
/// guild member's, else the cached user's), role mentions `@name` (not in DMs), channel mentions `#name`, slash
/// command mentions `/name`, custom emojis `:name:`; anything not cached stays as written. `guild_id` is the
/// message's guild (`None` in DMs). Reads the serenity cache; without a Discord connection only commands and emojis
/// are cleaned.
pub fn clean_content_in(app: &App, guild_id: Option<GuildId>, content: &str) -> String {
    let cache = app.discord().map(|d| d.cache.clone());
    CLEAN_RE.replace_all_with(content, |m, t| {
        let whole = String::from_utf16_lossy(&t[m.start..m.end]);
        if let Some(c) = m.group(t, 2) {
            return format!("/{c}");
        }
        if let Some(e) = m.group(t, 3) {
            return format!(":{e}:");
        }
        let (Some(kind), Some(id)) = (m.group(t, 1), m.group(t, 4).and_then(|s| s.parse::<u64>().ok())) else { return whole };
        let Some(cache) = cache.as_ref() else { return whole };
        if id == 0 {
            return whole;
        }
        match kind.as_str() {
            "@" | "@!" => {
                let uid = UserId::new(id);
                if let Some(name) = guild_id.and_then(|g| cache.guild(g).and_then(|g| g.members.get(&uid).map(member_display_name))) {
                    return format!("@{name}");
                }
                cache.user(uid).map(|u| format!("@{}", user_display_name(&u))).unwrap_or(whole)
            }
            "@&" => {
                let Some(g) = guild_id else { return whole };
                cache.guild(g).and_then(|g| g.roles.get(&serenity::all::RoleId::new(id)).map(|r| format!("@{}", r.name))).unwrap_or(whole)
            }
            "#" => {
                // `client.channels.cache`: any cached guild's channel or thread (the message's guild first).
                let cid = ChannelId::new(id);
                let in_guild = |g: GuildId| {
                    cache.guild(g).and_then(|g| g.channels.get(&cid).map(|c| c.name.clone()).or_else(|| g.threads.iter().find(|t| t.id == cid).map(|t| t.name.clone())))
                };
                let name = guild_id.and_then(in_guild).or_else(|| cache.guilds().into_iter().filter(|g| Some(*g) != guild_id).find_map(in_guild));
                name.map(|n| format!("#{n}")).unwrap_or(whole)
            }
            _ => whole,
        }
    })
}

/// `message.cleanContent`.
pub fn clean_content(app: &App, message: &serenity::all::Message) -> String {
    clean_content_in(app, message.guild_id, &message.content)
}
