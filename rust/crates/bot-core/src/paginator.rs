//! Port of `src/utils/paginator.js`: a paginated embed with Prev / Stop / Next buttons (`paginator:prev|stop|next`).
//! Only the invoker can page; other users are told so and cannot keep it alive. Clicks reach it through the app's
//! [`crate::collectors::CollectorHub`] (the router hands every `paginator` click to it).
//!
//! The page rendering (footers, buttons) is pure and tested against the JS; [`paginate`] sends the first page and
//! runs the collector in the background, catching everything.

use std::time::Duration;

use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, Message, ReactionType};
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed};

use crate::collectors::CollectorFilter;
use crate::context::App;
use crate::custom_embed::embed_from_json;
use crate::interaction::{InteractionError, Responder};
use crate::interaction_helpers::{Interactive, Payload, answer_stranger, clamp_collector_ms};

const PREV_ID: &str = "paginator:prev";
const NEXT_ID: &str = "paginator:next";
const STOP_ID: &str = "paginator:stop";
static PAGE_INDICATOR: LazyRe = lazy_re!(r"\bpage\s+\d+\s*(?:\/|of)\s*\d+", "i");
/// Interaction tokens live 15 minutes.
const TOKEN_FRESH_MS: i64 = 14 * 60_000;
const FOOTER_MAX: usize = 2048;

/// `opts.footer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FooterMode {
    /// Adds ` • Page X/N` to an existing footer (or sets `Page X of N`); a footer that already shows a page indicator
    /// is left alone.
    #[default]
    Append,
    /// Sets `Page X of N`.
    Replace,
    /// Leaves footers alone.
    None,
}

/// `paginate(target, pages, opts)` options.
#[derive(Debug, Clone)]
pub struct PaginateOptions {
    /// Collector idle timeout in ms (5 s to 14 min; default 60 s).
    pub timeout_ms: f64,
    /// Only for an interaction that is not yet acknowledged.
    pub ephemeral: bool,
    pub start_page: f64,
    /// Send with `followUp` instead of editing the original reply (after `deleteReply()`).
    pub follow_up: bool,
    pub footer: FooterMode,
}

impl Default for PaginateOptions {
    fn default() -> Self {
        Self { timeout_ms: 60_000.0, ephemeral: false, start_page: 0.0, follow_up: false, footer: FooterMode::Append }
    }
}

/// The pages snapshotted once (the caller's embeds are never changed), and the page rendering.
#[derive(Debug, Clone)]
pub struct Pages {
    base: Vec<Value>,
    mode: FooterMode,
}

impl Pages {
    /// Snapshots the embeds (as Discord JSON).
    pub fn new(pages: &[CreateEmbed], mode: FooterMode) -> Self {
        Self { base: pages.iter().map(|p| serde_json::to_value(p).unwrap_or_else(|_| json!({}))).collect(), mode }
    }

    /// From embed JSON directly.
    pub fn from_json(pages: Vec<Value>, mode: FooterMode) -> Self {
        Self { base: pages, mode }
    }

    pub fn len(&self) -> usize {
        self.base.len()
    }

    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
    }

    /// `footerFor(i)`.
    pub fn footer_for(&self, i: usize) -> Option<Value> {
        let n = self.base.len();
        let original = self.base[i].get("footer").filter(|f| !f.is_null()).cloned();
        if n == 1 || self.mode == FooterMode::None {
            return original;
        }
        let page_of = json!({ "text": format!("Page {} of {n}", i + 1) });
        if self.mode == FooterMode::Replace {
            return Some(page_of);
        }
        let Some(orig) = original else { return Some(page_of) };
        let Some(text) = orig.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) else { return Some(page_of) };
        if PAGE_INDICATOR.test(text) {
            return Some(orig);
        }
        let suffix = format!(" • Page {}/{n}", i + 1);
        let mut f = json!({ "text": format!("{}{suffix}", bot_util::text::clamp_text(text, FOOTER_MAX - bot_util::js::utf16_len(&suffix))) });
        if let Some(icon) = orig.get("icon_url").filter(|v| !v.is_null() && v.as_str() != Some("")) {
            f["icon_url"] = icon.clone();
        }
        Some(f)
    }

    /// `render(i)`: the page's embed JSON with its footer.
    pub fn render_json(&self, i: usize) -> Value {
        let mut e = self.base[i].clone();
        if let Some(f) = self.footer_for(i)
            && let Some(o) = e.as_object_mut()
        {
            o.insert("footer".into(), f);
        }
        e
    }

    /// `render(i)` as a serenity embed.
    pub fn render(&self, i: usize) -> CreateEmbed {
        embed_from_json(&self.render_json(i))
    }
}

/// `row(i, disabledAll)` as JSON (the buttons discord.js sends).
pub fn row_json(i: usize, n: usize, disabled_all: bool) -> Value {
    let b = |id: &str, emoji: &str, style: u8, disabled: bool| json!({ "type": 2, "emoji": { "name": emoji, "animated": false }, "custom_id": id, "style": style, "disabled": disabled });
    json!({ "type": 1, "components": [
        b(PREV_ID, "⬅️", 2, i == 0 || disabled_all),
        b(STOP_ID, "⏹️", 4, disabled_all),
        b(NEXT_ID, "➡️", 2, i + 1 == n || disabled_all),
    ]})
}

/// `row(i, disabledAll)`.
pub fn row(i: usize, n: usize, disabled_all: bool) -> CreateActionRow {
    let b = |id: &str, emoji: &str, style: ButtonStyle, disabled: bool| {
        CreateButton::new(id).emoji(ReactionType::Unicode(emoji.into())).style(style).disabled(disabled)
    };
    CreateActionRow::Buttons(vec![
        b(PREV_ID, "⬅️", ButtonStyle::Secondary, i == 0 || disabled_all),
        b(STOP_ID, "⏹️", ButtonStyle::Danger, disabled_all),
        b(NEXT_ID, "➡️", ButtonStyle::Secondary, i + 1 == n || disabled_all),
    ])
}

/// The first page index: `min(max(floor(startPage) || 0, 0), n - 1)`.
pub fn start_index(start: f64, n: usize) -> usize {
    let f = start.floor();
    let f = if f.is_finite() { f } else { 0.0 };
    (f.max(0.0) as usize).min(n.saturating_sub(1))
}

/// The page after a click (`prev`, `next`, `stop` keeps it).
pub fn next_page(page: usize, n: usize, custom_id: &str) -> usize {
    match custom_id {
        PREV_ID => page.saturating_sub(1),
        NEXT_ID => (page + 1).min(n.saturating_sub(1)),
        _ => page,
    }
}

/// Where the paginated message lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Original,
    FollowUp,
    Message,
}

/// `paginate(interaction, pages, opts)`: sends the first page and handles the buttons in the background. `Ok(None)`
/// when there are no pages; an error only when the first page cannot be sent.
pub async fn paginate(ctx: &impl Interactive, pages: &[CreateEmbed], opts: PaginateOptions) -> Result<Option<Message>, InteractionError> {
    if pages.is_empty() {
        return Ok(None);
    }
    let pages = Pages::new(pages, opts.footer);
    let n = pages.len();
    let page = start_index(opts.start_page, n);
    let payload = first_payload(&pages, page);
    let r = ctx.responder();
    let (message, mode) = if opts.follow_up && r.is_acknowledged() {
        (r.follow_up(payload.clone().ephemeral(opts.ephemeral).to_follow_up()).await?, Mode::FollowUp)
    } else if r.is_acknowledged() {
        (r.edit_reply(payload.to_edit()).await?, Mode::Original)
    } else {
        r.reply(payload.clone().ephemeral(opts.ephemeral).to_reply()).await?;
        (r.fetch_reply().await?, Mode::Original)
    };
    if n > 1 {
        let created = crate::interaction::created_ms(ctx.interaction_id()) as i64;
        spawn_collector(ctx.app().clone(), Some((r.clone(), created)), message.clone(), mode, pages, page, ctx.user().id, opts.timeout_ms);
    }
    Ok(Some(message))
}

/// `paginate(message, pages, opts)`: replies to a message in its channel (the invoker is its author).
#[allow(clippy::result_large_err)] // serenity's own error type
pub async fn paginate_message(app: &App, target: &Message, pages: &[CreateEmbed], opts: PaginateOptions) -> Result<Option<Message>, serenity::Error> {
    if pages.is_empty() {
        return Ok(None);
    }
    let Some(http) = app.http() else { return Err(serenity::Error::Other("no Discord connection")) };
    let pages = Pages::new(pages, opts.footer);
    let n = pages.len();
    let page = start_index(opts.start_page, n);
    let msg = first_payload(&pages, page).to_message().reference_message(target);
    let message = target.channel_id.send_message(http.as_ref(), msg).await?;
    if n > 1 {
        spawn_collector(app.clone(), None, message.clone(), Mode::Message, pages, page, target.author.id, opts.timeout_ms);
    }
    Ok(Some(message))
}

fn first_payload(pages: &Pages, page: usize) -> Payload {
    let components = if pages.len() > 1 { vec![row(page, pages.len(), false)] } else { Vec::new() };
    Payload::embed(pages.render(page)).components(components)
}

#[allow(clippy::too_many_arguments)]
fn spawn_collector(
    app: App,
    target: Option<(Responder, i64)>,
    message: Message,
    mode: Mode,
    pages: Pages,
    mut page: usize,
    invoker: serenity::all::UserId,
    timeout_ms: f64,
) {
    let idle = Duration::from_millis(clamp_collector_ms(timeout_ms, 60_000));
    let mut collector = app.collectors().collect(CollectorFilter::message(message.id).prefixes(["paginator"]));
    tokio::spawn(async move {
        let n = pages.len();
        let mut deadline = tokio::time::Instant::now() + idle;
        let mut last_click: Option<(Responder, i64)> = None;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let Some(click) = collector.next(left).await else { break };
            // Strangers neither page nor keep it alive (their clicks do not reset the idle timer).
            if click.user().id != invoker {
                answer_stranger(&click).await;
                continue;
            }
            deadline = tokio::time::Instant::now() + idle;
            last_click = Some((click.responder.clone(), crate::interaction::created_ms(click.interaction.id) as i64));
            let id = click.custom_id().to_string();
            if id == STOP_ID {
                let p = Payload::embed(pages.render(page)).components(vec![row(page, n, true)]);
                if let Err(e) = click.update(p.to_update()).await {
                    bot_util::log_debug!(format!("paginator collect failed: {e}"));
                }
                collector.stop();
                return;
            }
            page = next_page(page, n, &id);
            let p = Payload::embed(pages.render(page)).components(vec![row(page, n, false)]);
            if let Err(e) = click.update(p.to_update()).await {
                bot_util::log_debug!(format!("paginator collect failed: {e}"));
            }
        }
        collector.stop();
        // One edit when it idles out, through the interaction webhook whenever possible.
        let edit = Payload::new().components(vec![row(page, n, true)]);
        let now = app.clock().now_ms();
        let fresh = |created: i64| now - created < TOKEN_FRESH_MS;
        let click_fresh = last_click.as_ref().filter(|(_, c)| fresh(*c)).map(|(r, _)| r.clone());
        let target_fresh = target.as_ref().filter(|(_, c)| mode != Mode::Message && fresh(*c)).map(|(r, _)| r.clone());
        let result: Result<(), String> = if let Some(r) = click_fresh {
            // @original of a component interaction is this message.
            r.edit_reply(edit.to_edit()).await.map(|_| ()).map_err(|e| e.to_string())
        } else if let Some(r) = target_fresh {
            if mode == Mode::FollowUp {
                r.edit_follow_up(message.id, edit.to_follow_up()).await.map(|_| ()).map_err(|e| e.to_string())
            } else {
                r.edit_reply(edit.to_edit()).await.map(|_| ()).map_err(|e| e.to_string())
            }
        } else {
            match app.http() {
                Some(http) => message.channel_id.edit_message(http.as_ref(), message.id, edit.to_message_edit()).await.map(|_| ()).map_err(|e| e.to_string()),
                None => Err("no Discord connection".into()),
            }
        };
        if let Err(e) = result {
            bot_util::log_debug!(format!("paginator end edit failed: {e}"));
        }
    });
}
