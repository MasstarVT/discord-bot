//! Transcripts (§6.10): message normalisation, the self-contained HTML file (no scripts, images or remote loads; a
//! CSP meta blocks them too; every dynamic value escaped), the history reader and the summary embed.

use std::sync::LazyLock;

use bot_core::App;
use bot_core::adapters::user_tag;
use bot_core::custom_embed::is_http_url;
use bot_core::embed_builder::guild_embed;
use bot_core::permissions::permission_names;
use bot_db::GuildSettings;
use bot_util::jsre::JsRegex;
use bot_util::text::{clamp_text, format_number};
use bot_util::time::{discord_timestamp, is_valid_time_zone, zoned_parts_ms};
use serde::{Deserialize, Serialize};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Message, MessageId, MessageType, User, UserId};
use serenity::builder::{CreateAttachment, CreateEmbed};

use super::db::TicketFull;
use super::io::{io, sf};
use super::rules::{IoError, MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_MESSAGES, TRANSCRIPT_DEADLINE_MS, TRANSCRIPT_PAGE_SIZE, describe_discord_error, format_ticket_number};

/// `escapeHtml(s)`: `& < > " '` escaped.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// `formatBytes(n)`: '512 B', '1.2 KB', '3.4 MB' (base 1024, one decimal).
pub fn format_bytes(n: f64) -> String {
    let v = if n.is_finite() { n.max(0.0) } else { 0.0 };
    let k = 1024.0;
    if v < k {
        return format!("{} B", v.floor());
    }
    let fixed = to_fixed_1;
    if v < k * k {
        return format!("{} KB", fixed(v / k));
    }
    if v < k * k * k {
        return format!("{} MB", fixed(v / (k * k)));
    }
    format!("{} GB", fixed(v / (k * k * k)))
}

/// A transcript attachment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryAttachment {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// A field of a transcript embed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryField {
    pub name: String,
    pub value: String,
}

/// A transcript embed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryEmbed {
    pub title: String,
    pub description: String,
    pub fields: Vec<EntryField>,
    pub footer: String,
}

/// One message as a plain transcript entry (`normalizeMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub author_id: String,
    pub author_name: String,
    pub author_tag: String,
    pub bot: bool,
    pub created_timestamp: Option<i64>,
    pub edited_timestamp: Option<i64>,
    pub system: bool,
    pub system_label: String,
    pub content: String,
    pub reply_to_id: Option<String>,
    pub attachments: Vec<EntryAttachment>,
    pub embeds: Vec<EntryEmbed>,
    pub stickers: Vec<String>,
}

static MENTION_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"<@!?(\d+)>|<@&(\d+)>|<#(\d+)>|<a?:(\w+):\d+>|<t:(-?\d+)(?::[tTdDfFR])?>", "g"));

fn system_label(kind: MessageType) -> Option<&'static str> {
    Some(match kind {
        MessageType::PinsAdd => "pinned a message",
        MessageType::MemberJoin => "joined the server",
        MessageType::ThreadCreated => "started a thread",
        MessageType::GroupRecipientAddition => "added someone",
        MessageType::GroupRecipientRemoval => "removed someone",
        MessageType::GroupNameUpdate => "changed the channel name",
        MessageType::GroupIconUpdate => "changed the channel icon",
        _ => return None,
    })
}

/// discord.js `message.system`: every type but Default, Reply, ChatInputCommand and ContextMenuCommand.
pub fn is_system(kind: MessageType) -> bool {
    !matches!(kind, MessageType::Regular | MessageType::InlineReply | MessageType::ChatInputCommand | MessageType::ContextMenuCommand)
}

fn display_of(nick: Option<&str>, user: &User) -> String {
    nick.filter(|n| !n.is_empty()).map(str::to_string).or_else(|| user.global_name.clone()).unwrap_or_else(|| user.name.clone())
}

/// `normalizeMessage(message, guild)`: mentions and emoji are resolved to text in a single pass, so a substituted name
/// is never expanded again.
pub fn normalize_message(message: &Message, guild: Option<&Guild>) -> Entry {
    let resolved = MENTION_RE.replace_all_with(&message.content, |m, t| {
        let g = |i| m.group(t, i);
        if let Some(user) = g(1) {
            let mentioned = message.mentions.iter().find(|u| u.id.to_string() == user);
            let name = mentioned
                .and_then(|u| u.member.as_ref().map(|pm| display_of(pm.nick.as_deref(), u)))
                .or_else(|| mentioned.map(|u| u.name.clone()))
                .or_else(|| {
                    let id = UserId::new(sf(&user)?);
                    guild.and_then(|g| g.members.get(&id)).map(|mem| display_of(mem.nick.as_deref(), &mem.user))
                })
                .unwrap_or(user);
            return format!("@{name}");
        }
        if let Some(role) = g(2) {
            let name = sf(&role).and_then(|r| guild.and_then(|g| g.roles.get(&r.into()).map(|x| x.name.clone()))).unwrap_or_else(|| "deleted-role".into());
            return format!("@{name}");
        }
        if let Some(chan) = g(3) {
            let name = sf(&chan).and_then(|c| guild.and_then(|g| g.channels.get(&ChannelId::new(c)).map(|x| x.name.clone()))).unwrap_or_else(|| "deleted-channel".into());
            return format!("#{name}");
        }
        if let Some(emoji) = g(4) {
            return format!(":{emoji}:");
        }
        let whole = m.text(t);
        let stamp: f64 = g(5).and_then(|s| s.parse().ok()).unwrap_or(f64::NAN);
        bot_util::js::to_iso_string(stamp * 1000.0).unwrap_or(whole)
    });
    let author = &message.author;
    let system = is_system(message.kind);
    Entry {
        id: message.id.to_string(),
        author_id: author.id.to_string(),
        author_name: display_of(message.member.as_ref().and_then(|m| m.nick.as_deref()), author),
        author_tag: user_tag(author),
        bot: author.bot,
        created_timestamp: Some(message.timestamp.timestamp_millis()),
        edited_timestamp: message.edited_timestamp.map(|t| t.timestamp_millis()),
        system,
        system_label: if system { system_label(message.kind).unwrap_or("system message").to_string() } else { String::new() },
        content: clamp_text(&resolved, 4000),
        reply_to_id: message.message_reference.as_ref().and_then(|r| r.message_id).map(|m| m.to_string()),
        attachments: message
            .attachments
            .iter()
            .filter(|a| is_http_url(&a.url))
            .map(|a| EntryAttachment { name: clamp_text(&a.filename, 200), url: a.url.clone(), size: u64::from(a.size) })
            .collect(),
        embeds: message
            .embeds
            .iter()
            .map(|e| EntryEmbed {
                title: clamp_text(e.title.as_deref().unwrap_or(""), 256),
                description: clamp_text(e.description.as_deref().unwrap_or(""), 4096),
                fields: e.fields.iter().take(25).map(|f| EntryField { name: clamp_text(&f.name, 256), value: clamp_text(&f.value, 1024) }).collect(),
                footer: clamp_text(e.footer.as_ref().map(|f| f.text.as_str()).unwrap_or(""), 2048),
            })
            .collect(),
        stickers: message.sticker_items.iter().map(|s| clamp_text(&s.name, 100)).collect(),
    }
}

const TRANSCRIPT_CSS: &str = concat!(
    "body{background:#1e1f22;color:#dbdee1;font:14px/1.4 sans-serif;margin:0;padding:16px}h1{font-size:20px}",
    "dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 12px;color:#949ba4}dd{margin:0;color:#dbdee1}",
    ".msg{padding:6px 0;border-top:1px solid #2b2d31}.meta{color:#949ba4;font-size:12px}.author{color:#fff;font-weight:600;font-size:14px}",
    ".content{white-space:pre-wrap;word-wrap:break-word}.badge{background:#5865f2;color:#fff;border-radius:3px;padding:0 4px;font-size:10px}",
    ".embed{border-left:4px solid #5865f2;background:#2b2d31;padding:6px 10px;margin:4px 0;white-space:pre-wrap}.et{font-weight:600}",
    ".sys,.att,.reply,.note{color:#949ba4;font-size:13px}a{color:#00a8fc}",
);

/// Ticket and server details of a transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptMeta {
    pub num: String,
    pub guild_name: String,
    pub channel_name: String,
    pub panel_name: String,
    pub owner_tag: String,
    pub owner_id: String,
    pub opened_at: Option<f64>,
    pub claimed_tag: String,
    pub closed_at: Option<f64>,
    pub reason: String,
    /// `null`, `'limit'` or `'time'`.
    pub truncated: Option<String>,
    pub generated_at: Option<f64>,
    pub generated_by_tag: String,
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sept", "Oct", "Nov", "Dec"];

/// `new Intl.DateTimeFormat('en-GB', { timeZone, dateStyle: 'medium', timeStyle: 'short' }).format(ms)`.
pub fn format_en_gb(ms: f64, tz: &str) -> String {
    let p = zoned_parts_ms(ms, tz);
    let month = MONTHS.get((p.month as usize).saturating_sub(1)).copied().unwrap_or("Jan");
    format!("{} {month} {}, {:02}:{:02}", p.day, p.year, p.hour, p.minute)
}

/// The rendered transcript: the HTML, and how many messages it holds and left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTranscript {
    pub html: String,
    pub included: usize,
    pub omitted: usize,
}

/// `renderTranscriptHtml(meta, entries, { timeZone, maxBytes })`: when the file would exceed `max_bytes` the oldest
/// messages are dropped and the header says so.
pub fn render_transcript_html(meta: &TranscriptMeta, entries: &[Entry], time_zone: &str, max_bytes: usize) -> RenderedTranscript {
    let tz = if is_valid_time_zone(time_zone) { time_zone.to_string() } else { "UTC".to_string() };
    let fmt_time = |ms: Option<f64>| match ms {
        Some(v) if v.is_finite() && bot_util::js::to_iso_string(v).is_some() => format_en_gb(v, &tz),
        _ => "—".to_string(),
    };
    let iso_of = |ms: Option<f64>| bot_util::js::to_iso_string(ms.unwrap_or(0.0)).unwrap_or_default();
    let esc = escape_html;
    let num = esc(&meta.num);

    let article = |e: &Entry| -> String {
        let mut h = format!(
            "<article class=\"msg{}{}\" id=\"m-{}\"><div class=\"meta\"><span class=\"author\">{}</span> <span class=\"tag\">{} · {}</span>{} <time datetime=\"{}\">{}</time>{}</div>",
            if e.bot { " bot" } else { "" },
            if e.system { " sys" } else { "" },
            esc(&e.id),
            esc(&e.author_name),
            esc(&e.author_tag),
            esc(&e.author_id),
            if e.bot { " <span class=\"badge\">BOT</span>" } else { "" },
            esc(&iso_of(e.created_timestamp.map(|t| t as f64))),
            esc(&fmt_time(e.created_timestamp.map(|t| t as f64))),
            match e.edited_timestamp {
                Some(t) if t != 0 => format!(" <span class=\"edited\">(edited {})</span>", esc(&fmt_time(Some(t as f64)))),
                _ => String::new(),
            },
        );
        if let Some(r) = e.reply_to_id.as_deref().filter(|r| !r.is_empty()) {
            h.push_str(&format!("<div class=\"reply\">↪ <a href=\"#m-{}\">reply</a></div>", esc(r)));
        }
        if e.system {
            h.push_str(&format!("<div class=\"sys\">{}</div>", esc(if e.system_label.is_empty() { "system message" } else { &e.system_label })));
        }
        if !e.content.is_empty() {
            h.push_str(&format!("<div class=\"content\">{}</div>", esc(&e.content)));
        }
        for em in &e.embeds {
            h.push_str("<div class=\"embed\">");
            if !em.title.is_empty() {
                h.push_str(&format!("<div class=\"et\">{}</div>", esc(&em.title)));
            }
            if !em.description.is_empty() {
                h.push_str(&format!("<div class=\"ed\">{}</div>", esc(&em.description)));
            }
            for f in &em.fields {
                h.push_str(&format!("<div class=\"ef\"><b>{}</b><div>{}</div></div>", esc(&f.name), esc(&f.value)));
            }
            if !em.footer.is_empty() {
                h.push_str(&format!("<div class=\"eft\">{}</div>", esc(&em.footer)));
            }
            h.push_str("</div>");
        }
        for a in &e.attachments {
            let size = if a.size != 0 { format!(" ({})", format_bytes(a.size as f64)) } else { String::new() };
            if is_http_url(&a.url) {
                h.push_str(&format!("<div class=\"att\">📎 <a href=\"{}\" rel=\"noopener noreferrer nofollow\">{}</a>{size}</div>", esc(&a.url), esc(&a.name)));
            } else {
                h.push_str(&format!("<div class=\"att\">📎 {}{size}</div>", esc(&a.name)));
            }
        }
        for s in &e.stickers {
            h.push_str(&format!("<div class=\"att\">🏷️ Sticker: {}</div>", esc(s)));
        }
        format!("{h}</article>\n")
    };

    let closed = meta.closed_at.filter(|c| *c != 0.0 && !c.is_nan()).map(|c| fmt_time(Some(c))).unwrap_or_else(|| "—".into());
    let head = |omitted: usize| -> String {
        let mut notes = String::new();
        if omitted > 0 {
            notes.push_str(&format!("<p class=\"note\">{omitted} earlier messages are not included (file size limit).</p>"));
        }
        if meta.truncated.as_deref() == Some("limit") {
            notes.push_str(&format!("<p class=\"note\">Only the last {MAX_TRANSCRIPT_MESSAGES} messages are included.</p>"));
        }
        if meta.truncated.as_deref() == Some("time") {
            notes.push_str("<p class=\"note\">The transcript stopped early (time limit); older messages are missing.</p>");
        }
        let or_dash = |s: &str| if s.is_empty() { "—".to_string() } else { s.to_string() };
        format!(
            "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>Ticket #{num} · {gn} — transcript</title>\n<style>{TRANSCRIPT_CSS}</style>\n</head><body>\n<header><h1>Ticket #{num}</h1><dl>\n<dt>Server</dt><dd>{gn}</dd><dt>Channel</dt><dd>#{cn}</dd><dt>Panel</dt><dd>{pn}</dd>\n<dt>Opened by</dt><dd>{ot} ({oi})</dd><dt>Opened</dt><dd>{opened}</dd>\n<dt>Claimed by</dt><dd>{ct}</dd><dt>Closed</dt><dd>{closed}</dd><dt>Reason</dt><dd>{reason}</dd>\n<dt>Times</dt><dd>{tz}</dd>\n</dl>{notes}\n</header><main>\n",
            gn = esc(&meta.guild_name),
            cn = esc(&meta.channel_name),
            pn = esc(&or_dash(&meta.panel_name)),
            ot = esc(&meta.owner_tag),
            oi = esc(&meta.owner_id),
            opened = esc(&fmt_time(meta.opened_at)),
            ct = esc(&or_dash(&meta.claimed_tag)),
            closed = esc(&closed),
            reason = esc(&or_dash(&meta.reason)),
            tz = esc(&tz),
        )
    };
    let foot = |included: usize| -> String {
        format!(
            "</main><footer>Generated {} by {} · {included} message(s) · Attachment links point to Discord and may stop working.</footer>\n</body></html>\n",
            esc(&fmt_time(meta.generated_at)),
            esc(&meta.generated_by_tag)
        )
    };

    let articles: Vec<String> = entries.iter().map(article).collect();
    let fixed = head(entries.len()).len() + foot(entries.len()).len() + 512;
    let mut sum = 0;
    let mut start = entries.len();
    for i in (0..entries.len()).rev() {
        let b = articles[i].len();
        if fixed + sum + b > max_bytes {
            break;
        }
        sum += b;
        start = i;
    }
    let included = entries.len() - start;
    let omitted = start;
    RenderedTranscript { html: format!("{}{}{}", head(omitted), articles[start..].concat(), foot(included)), included, omitted }
}

/// [`render_transcript_html`] with the JS defaults (`UTC`, 8 MiB).
pub fn render_transcript_html_default(meta: &TranscriptMeta, entries: &[Entry]) -> RenderedTranscript {
    render_transcript_html(meta, entries, "UTC", MAX_TRANSCRIPT_BYTES)
}

/// `fetchTranscriptMessages(channel, { max, deadlineMs })`: the channel history (100 per request, newest first),
/// returned chronologically, and whether it was cut (`limit` / `time`).
pub async fn fetch_transcript_messages(app: &App, channel_id: ChannelId, max: usize, deadline_ms: u64) -> Result<(Vec<Message>, Option<&'static str>), IoError> {
    let mut out: Vec<Message> = Vec::new();
    let mut before: Option<MessageId> = None;
    let mut truncated = None;
    let start = app.clock().now_ms();
    let io = io(app);
    loop {
        let batch = io.fetch_messages(channel_id, before, TRANSCRIPT_PAGE_SIZE).await?;
        let full = batch.len() >= usize::from(TRANSCRIPT_PAGE_SIZE);
        let last = batch.last().map(|m| m.id);
        out.extend(batch);
        if out.len() > max || (out.len() == max && full) {
            truncated = Some("limit");
            break;
        }
        if !full {
            break;
        }
        if app.clock().now_ms() - start > deadline_ms as i64 {
            truncated = Some("time");
            break;
        }
        before = last;
    }
    out.truncate(max);
    out.reverse();
    Ok((out, truncated))
}

/// A built transcript: the file (twice: the channel copy and the DM copy), its summary embed and counts.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub file: CreateAttachment,
    pub dm_file: CreateAttachment,
    pub html: String,
    pub embed: CreateEmbed,
    pub count: usize,
    pub truncated: Option<&'static str>,
    pub omitted: usize,
}

fn ms_of(dt: &chrono::DateTime<chrono::Utc>) -> f64 {
    dt.timestamp_millis() as f64
}

/// The transcript embed (`📄 Ticket #0001 transcript` with its fields).
pub fn transcript_embed(t: &TicketFull, settings: Option<&GuildSettings>, deleted_by: Option<&str>, included: usize, omitted: usize) -> CreateEmbed {
    let tk = &t.ticket;
    let num = format_ticket_number(i64::from(tk.number));
    let mut e = guild_embed(settings, Some(&format!("📄 Ticket #{num} transcript")), None, None)
        .field("Opened by", format!("<@{}>", tk.owner_id), true)
        .field("Claimed by", tk.claimed_by_id.as_ref().map(|c| format!("<@{c}>")).unwrap_or_else(|| "—".into()), true)
        .field("Closed by", tk.closed_by_id.as_ref().map(|c| format!("<@{c}>")).unwrap_or_else(|| "—".into()), true);
    if let Some(d) = deleted_by {
        e = e.field("Deleted by", format!("<@{d}>"), true);
    }
    let panel_name = t.panel.as_ref().map(|p| p.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "—".into());
    let messages = if omitted > 0 {
        format!("{} (+{} omitted)", format_number(included as f64), format_number(omitted as f64))
    } else {
        format_number(included as f64)
    };
    e = e
        .field("Panel", clamp_text(&panel_name, 1024), true)
        .field("Opened", discord_timestamp(&tk.created_at, "f"), true)
        .field("Closed", tk.closed_at.as_ref().map(|c| discord_timestamp(c, "f")).unwrap_or_else(|| "—".into()), true)
        .field("Messages", messages, true);
    if let Some(r) = tk.reason.as_deref().filter(|r| !r.is_empty()) {
        e = e.field("Reason", clamp_text(r, 1024), false);
    }
    e
}

/// Who generated a transcript (`generatedBy` / `deletedBy`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub tag: String,
}

/// `buildTranscript({ guild, ticket, channel, settings, generatedBy, deletedBy })`: the file and its summary embed, or a
/// short reason.
pub async fn build_transcript(
    app: &App,
    guild_id: GuildId,
    t: &TicketFull,
    channel: &GuildChannel,
    settings: Option<&GuildSettings>,
    generated_by: Option<&Person>,
    deleted_by: Option<&Person>,
) -> Result<Transcript, String> {
    let missing = super::io::missing_in_channel(app, guild_id, channel, &["ViewChannel", "ReadMessageHistory"]);
    if !missing.is_empty() {
        return Err(format!("missing {}", permission_names(&missing).join(", ")));
    }
    let (messages, truncated) = match fetch_transcript_messages(app, channel.id, MAX_TRANSCRIPT_MESSAGES, TRANSCRIPT_DEADLINE_MS).await {
        Ok(v) => v,
        Err(e) => {
            bot_util::log_warn!(format!("tickets: building a transcript failed: {}", e.message));
            return Err(describe_discord_error(&e));
        }
    };
    let guild = bot_core::interaction_helpers::cached_guild(app, guild_id);
    let entries: Vec<Entry> = messages.iter().map(|m| normalize_message(m, guild.as_ref())).collect();
    let tk = &t.ticket;
    let owner = match sf(&tk.owner_id) {
        Some(id) => tokio::time::timeout(std::time::Duration::from_millis(3000), io(app).fetch_user(UserId::new(id))).await.ok().and_then(Result::ok),
        None => None,
    };
    let claimed = tk.claimed_by_id.as_deref().and_then(sf).and_then(|id| guild.as_ref().and_then(|g| g.members.get(&UserId::new(id)).map(|m| user_tag(&m.user))));
    let num = format_ticket_number(i64::from(tk.number));
    let tz = settings.map(|s| s.timezone.clone()).filter(|z| is_valid_time_zone(z)).unwrap_or_else(|| "UTC".into());
    let meta = TranscriptMeta {
        num: num.clone(),
        guild_name: guild.as_ref().map(|g| g.name.clone()).unwrap_or_default(),
        channel_name: channel.name.clone(),
        panel_name: t.panel.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
        owner_tag: owner.as_ref().map(user_tag).unwrap_or_else(|| tk.owner_id.clone()),
        owner_id: tk.owner_id.clone(),
        opened_at: Some(ms_of(&tk.created_at)),
        claimed_tag: if tk.claimed_by_id.is_some() { claimed.unwrap_or_else(|| tk.claimed_by_id.clone().unwrap_or_default()) } else { String::new() },
        closed_at: tk.closed_at.as_ref().map(ms_of),
        reason: tk.reason.clone().unwrap_or_default(),
        truncated: truncated.map(str::to_string),
        generated_at: Some(app.clock().now_ms() as f64),
        generated_by_tag: generated_by.map(|p| p.tag.clone()).unwrap_or_else(|| "the bot".into()),
    };
    let r = render_transcript_html(&meta, &entries, &tz, MAX_TRANSCRIPT_BYTES);
    let name = format!("transcript-ticket-{num}.html");
    let description = format!("Ticket #{num} transcript");
    let file = CreateAttachment::bytes(r.html.clone().into_bytes(), name).description(description);
    let embed = transcript_embed(t, settings, deleted_by.map(|p| p.id.as_str()), r.included, r.omitted);
    Ok(Transcript { dm_file: file.clone(), file, html: r.html, embed, count: r.included, truncated, omitted: r.omitted })
}

/// `Number.prototype.toFixed(1)`: the nearest one-decimal value, a tie going to the larger one.
pub fn to_fixed_1(x: f64) -> String {
    let scaled = x * 10.0;
    if scaled.fract() == 0.5 {
        return format!("{:.1}", scaled.ceil() / 10.0);
    }
    format!("{x:.1}")
}
