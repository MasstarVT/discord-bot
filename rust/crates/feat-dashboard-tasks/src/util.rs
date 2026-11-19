//! Small JS-semantics and Discord-lookup helpers shared by the task modules (the parts of the JS files that are
//! repeated in each of them: `SNOWFLAKE_RE`, `Number.isInteger`, `String(x ?? '')`, `guild.members.fetch`, ...).

use bot_config::PermissionLevel;
use bot_core::App;
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_db::GuildSettings;
use serde_json::Value;
use serenity::all::{ChannelId, GuildChannel, GuildId, Member, RoleId, User, UserId};

/// `/^\d{17,20}$/`.
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `typeof v === 'string' && SNOWFLAKE_RE.test(v)`.
pub fn is_id(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(is_snowflake)
}

/// `Number.isInteger(v)`: the number when it is a finite whole number.
pub fn js_integer(v: Option<&Value>) -> Option<f64> {
    let n = v?.as_f64()?;
    (n.is_finite() && n.fract() == 0.0).then_some(n)
}

/// `Number.isSafeInteger(v)`.
pub fn js_safe_integer(v: Option<&Value>) -> Option<f64> {
    js_integer(v).filter(|n| n.abs() <= 9_007_199_254_740_991.0)
}

/// `v === undefined || v === null`.
pub fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// `String(v)` for a JSON value (`undefined` when absent).
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::Array(a)) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `String(v ?? '')`.
pub fn js_string_or_empty(v: Option<&Value>) -> String {
    if is_nullish(v) { String::new() } else { js_string(v) }
}

/// `value !== null && typeof value === 'object' && !Array.isArray(value)`.
pub fn is_object(v: &Value) -> bool {
    v.is_object()
}

/// `n.toLocaleString('en-US')` for a whole number.
pub fn fmt(n: f64) -> String {
    bot_util::js::to_locale_en_us(n, 3)
}

/// `d.toISOString().slice(0, 16).replace('T', ' ') + ' UTC'`.
pub fn utc(d: chrono::DateTime<chrono::Utc>) -> String {
    format!("{} UTC", d.format("%Y-%m-%d %H:%M"))
}

pub fn gid(s: &str) -> Option<GuildId> {
    s.parse::<u64>().ok().filter(|v| *v != 0).map(GuildId::new)
}

pub fn cid(s: &str) -> Option<ChannelId> {
    s.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
}

pub fn uid(s: &str) -> Option<UserId> {
    s.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new)
}

pub fn rid(s: &str) -> Option<RoleId> {
    s.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new)
}

/// `guild.members.cache.get(id)`.
pub fn cached_member(app: &App, guild_id: &str, user_id: &str) -> Option<Member> {
    let (g, u) = (gid(guild_id)?, uid(user_id)?);
    app.discord()?.cache.guild(g)?.members.get(&u).cloned()
}

/// `guild.members.fetch(id).catch(() => null)`: the cache, then REST.
pub async fn fetch_member(app: &App, guild_id: &str, user_id: &str) -> Option<Member> {
    if let Some(m) = cached_member(app, guild_id, user_id) {
        return Some(m);
    }
    let (g, u) = (gid(guild_id)?, uid(user_id)?);
    let http = app.http()?;
    http.get_member(g, u).await.ok()
}

/// `client.users.cache.get(id) ?? await client.users.fetch(id).catch(() => null)`.
pub async fn fetch_user(app: &App, user_id: &str) -> Option<User> {
    let u = uid(user_id)?;
    if let Some(d) = app.discord()
        && let Some(user) = d.cache.user(u)
    {
        return Some(user.clone());
    }
    let http = app.http()?;
    http.get_user(u).await.ok()
}

/// discord.js `member.displayName`: the nickname, else the global name, else the username.
pub fn member_display_name(member: &Member) -> String {
    member.nick.clone().filter(|n| !n.is_empty()).or_else(|| member.user.global_name.clone().filter(|n| !n.is_empty())).unwrap_or_else(|| member.user.name.clone())
}

/// `user.globalName || user.username`.
pub fn user_name(user: &User) -> String {
    user.global_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| user.name.clone())
}

/// `guild.channels.cache.get(id)?.name`.
pub fn cached_channel_name(app: &App, guild_id: &str, channel_id: &str) -> Option<String> {
    let (g, c) = (gid(guild_id)?, cid(channel_id)?);
    let guild = app.discord()?.cache.guild(g)?;
    guild.channels.get(&c).map(|ch| ch.name.clone()).or_else(|| guild.threads.iter().find(|t| t.id == c).map(|t| t.name.clone()))
}

/// `guild.channels.cache.get(id)`.
pub fn cached_channel(app: &App, guild_id: &str, channel_id: &str) -> Option<GuildChannel> {
    let (g, c) = (gid(guild_id)?, cid(channel_id)?);
    bot_core::interaction_helpers::cached_channel(app, g, c)
}

/// `guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null)`.
pub async fn find_channel(app: &App, guild_id: &str, channel_id: &str) -> Option<GuildChannel> {
    let (g, c) = (gid(guild_id)?, cid(channel_id)?);
    bot_core::interaction_helpers::resolve_channel(app, g, c).await
}

/// `guild.roles.cache.get(id)?.name`.
pub fn cached_role_name(app: &App, guild_id: &str, role_id: &str) -> Option<String> {
    let (g, r) = (gid(guild_id)?, rid(role_id)?);
    app.discord()?.cache.guild(g)?.roles.get(&r).map(|role| role.name.clone())
}

/// `guild.roles.cache.has(id)`.
pub fn role_cached(app: &App, guild_id: &str, role_id: &str) -> bool {
    cached_role_name(app, guild_id, role_id).is_some()
}

/// `guild.roles.cache.get(id) ?? await guild.roles.fetch(id).catch(() => null)`: the role's name when it exists.
pub async fn fetch_role_name(app: &App, guild_id: &str, role_id: &str) -> Option<String> {
    if let Some(name) = cached_role_name(app, guild_id, role_id) {
        return Some(name);
    }
    let (g, r) = (gid(guild_id)?, rid(role_id)?);
    let http = app.http()?;
    http.get_guild_role(g, r).await.ok().map(|role| role.name)
}

/// `resolvePermissionLevel(member, settings)`, with guild-level permissions from the cache.
pub fn member_level(app: &App, guild_id: &str, member: &Member, settings: Option<&GuildSettings>) -> PermissionLevel {
    let g = gid(guild_id).unwrap_or(member.guild_id);
    let access = MemberAccess::from_cache(app.discord().map(|d| d.cache.as_ref()), g, member);
    resolve_permission_level(Some(&access), settings, app.config().bot_owner_id.as_deref())
}

/// `str.replace(/\*\*/g, '')`.
pub fn strip_bold(s: &str) -> String {
    s.replace("**", "")
}

/// Replaces every `<#digits>` in `s` with `f(digits)`.
pub fn replace_channel_mentions(s: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<#") {
        let after = &rest[i + 2..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after.as_bytes().get(digits) == Some(&b'>') {
            out.push_str(&rest[..i]);
            out.push_str(&f(&after[..digits]));
            rest = &after[digits + 1..];
        } else {
            out.push_str(&rest[..i + 2]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `s.slice(0, n)` in UTF-16 units.
pub fn slice16(s: &str, n: usize) -> String {
    bot_util::js::utf16_prefix(s, n).to_string()
}

/// `s.length` in UTF-16 units.
pub fn len16(s: &str) -> usize {
    bot_util::js::utf16_len(s)
}
