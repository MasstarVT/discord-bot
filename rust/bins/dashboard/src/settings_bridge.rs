//! Port of `src/dashboard/settingsBridge.js`: the dashboard's only path to GuildSettings. Reads and writes go through
//! bot-core's settings service (the single settings gateway), stored JSON is read through the bot's own normalisers,
//! and writes repeat the side effects the matching slash commands have. What cannot be done without the bot's gateway
//! connection is left to the commands and said so in the field descriptions (dash-schema's fields).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::App;
use bot_core::scheduler::CancelJobs;
use bot_core::settings::SettingsPatch;
use bot_db::GuildSettings;
use dash_collections::collections::embeds::render_context_for;
use dash_collections::{DashboardDiscord, DiscordResources, SettingsBridge, SettingsWriteCtx, SettingsWriteError, SettingsWritten};
use dash_schema::{FieldError, check_cross_fields, row_json, to_stored_changes};
use serde_json::{Map, Value, json};

/// `voiceSessionKeys(guildId)`: the open voice sessions of a guild.
pub fn voice_session_keys(guild_id: &str) -> String {
    format!("voice:start:{guild_id}:*")
}

/// `BIRTHDAY_SCHEDULE_FIELDS`.
pub const BIRTHDAY_SCHEDULE_FIELDS: [&str; 3] = ["birthdayChannelId", "birthdayRoleId", "birthdayHour"];

/// `normalizeRow(row)`: a GuildSettings row as JSON with its JSON columns in the shapes the dashboard fields read: the
/// automod config upgraded to v2, the welcome embeds healed, the welcome card flag resolved (a legacy
/// `welcomeEmbed.card` included).
pub fn normalize_row(row: &GuildSettings) -> Value {
    let mut v = row_json(row);
    let Some(obj) = v.as_object_mut() else { return v };
    obj.insert("automodConfig".into(), feat_automod::automod_service::normalize_config(row.automod_config.as_ref()).to_json());
    let mut style = feat_welcome::welcome_service::parse_card_style(row.welcome_card_style.as_ref());
    style.enabled = Some(feat_welcome::welcome_service::welcome_card_enabled(Some(row)));
    obj.insert("welcomeCardStyle".into(), feat_welcome::welcome_service::serialize_card_style(&style));
    for (key, raw) in [("welcomeEmbed", &row.welcome_embed), ("welcomeDmEmbed", &row.welcome_dm_embed), ("goodbyeEmbed", &row.goodbye_embed)] {
        obj.insert(key.into(), feat_welcome::welcome_service::heal_embed(raw.as_ref()).unwrap_or(Value::Null));
    }
    v
}

/// `JSON.stringify(value)` for the change comparison (`undefined` stays `None`).
fn stringify(v: Option<&Value>) -> Option<String> {
    v.map(|v| serde_json::to_string(v).unwrap_or_default())
}

/// The settings bridge over the app's settings service, Redis and scheduler.
#[derive(Clone)]
pub struct DashboardSettings {
    app: App,
}

impl DashboardSettings {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    /// `readGuildSettings(guildId)` typed: the current row, bypassing this process's 30 s memo (the bot may have
    /// changed it).
    pub async fn read_row(&self, guild_id: &str) -> anyhow::Result<Arc<GuildSettings>> {
        Ok(self.app.settings().get_settings_with(guild_id, true).await?)
    }

    /// Downloads a new card background once, as /welcome card and /levelconfig card do, so a broken link is refused.
    async fn check_backgrounds(changes: &Map<String, Value>, current: &Value) -> Result<(), FieldError> {
        for field in ["welcomeCardStyle", "rankCardStyle"] {
            let Some(url) = changes.get(field).and_then(|c| c.get("backgroundUrl")).and_then(Value::as_str) else { continue };
            if current.get(field).and_then(|c| c.get("backgroundUrl")).and_then(Value::as_str) == Some(url) {
                continue;
            }
            let refused = if field == "welcomeCardStyle" {
                match bot_cards::welcome_card::validate_card_background(Some(url)).await {
                    bot_cards::welcome_card::BackgroundCheck::Ok => None,
                    bot_cards::welcome_card::BackgroundCheck::Invalid { reason } => Some(reason),
                }
            } else {
                match bot_cards::profile_card::validate_card_background(url).await {
                    bot_cards::profile_card::BackgroundCheck::Ok => None,
                    bot_cards::profile_card::BackgroundCheck::Invalid { reason } => Some(reason),
                }
            };
            if let Some(reason) = refused {
                return Err(FieldError::new(format!("{field}.backgroundUrl"), format!("That background image cannot be used: {reason}")));
            }
        }
        Ok(())
    }

    /// `writeSettings(guildId, changes, { discord, logger })`: saves validated changes in one row-locked
    /// read-modify-write, with the cross-field rules checked against the merged row, then repeats the commands' side
    /// effects:
    ///   - voice XP or the Levels module switched off: open voice sessions are dropped without credit (/levelconfig);
    ///   - the captcha kick timer set to 0: the scheduled kicks are cancelled (/captcha timeout);
    ///   - the panel message or Verify button changed: the posted panel is edited (/captcha message);
    ///   - birthday channel, role or hour changed: the birthday job is rescheduled (/birthday config);
    ///   - the maximum balance lowered: balances above it are lowered (/economy-admin config limits);
    ///   - a new moderation log channel: a confirmation is posted there (/modconfig logchannel);
    ///   - Auto-Moderation switched on: the stored config is rewritten in the current format (/automod enable);
    ///   - the welcome embed changed: the welcome text that was in effect is kept (materialiseWelcome).
    ///
    /// A failed side effect never undoes the save: it is logged.
    pub async fn write(&self, guild_id: &str, changes: Map<String, Value>, discord: Option<Arc<dyn DashboardDiscord>>) -> Result<SettingsWritten, SettingsWriteError> {
        let stored = normalize_row(&*self.read_row(guild_id).await?);
        Self::check_backgrounds(&changes, &stored).await?;

        let before = Arc::new(Mutex::new(stored));
        let (b2, ch) = (before.clone(), changes.clone());
        let written = self
            .app
            .settings()
            .update_settings_with(guild_id, move |cur| {
                let current = normalize_row(&cur);
                *b2.lock().unwrap_or_else(|e| e.into_inner()) = current.clone();
                let mut data = to_stored_changes(&ch, Some(&current));
                if ch.get("automodEnabled") == Some(&Value::Bool(true)) && !data.contains_key("automodConfig") {
                    data.insert("automodConfig".into(), current.get("automodConfig").cloned().unwrap_or(Value::Null));
                }
                let mut patch = SettingsPatch(data);
                if patch.0.contains_key("welcomeEmbed") {
                    feat_welcome::welcome_service::materialise_welcome(&cur, &mut patch);
                }
                let mut merged = current.as_object().cloned().unwrap_or_default();
                for (k, v) in patch.iter() {
                    merged.insert(k.clone(), v.clone());
                }
                if let Some(problem) = check_cross_fields(Some(&Value::Object(merged)), Some(&ch)) {
                    return Err(anyhow::Error::new(FieldError::new(problem.path, problem.message)));
                }
                Ok(patch)
            })
            .await
            .map_err(|e| match e.downcast::<FieldError>() {
                Ok(f) => SettingsWriteError::Field(f),
                Err(e) => SettingsWriteError::Internal(e),
            })?;
        let row = normalize_row(&written);
        let before = before.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let mut notices = Vec::new();
        let changed = |field: &str| changes.contains_key(field) && stringify(before.get(field)) != stringify(row.get(field));
        let warn = |name: &str, err: &dyn std::fmt::Display| {
            bot_util::log_warn!(format!("Dashboard: {name} after a settings change in guild {guild_id} failed: {err}"));
        };

        if changes.get("voiceXpEnabled") == Some(&Value::Bool(false)) || changes.get("xpEnabled") == Some(&Value::Bool(false)) {
            self.app.cache().invalidate(&voice_session_keys(guild_id)).await;
        }
        if changes.contains_key("captchaTimeoutMinutes") && written.captcha_timeout_minutes == 0 {
            let filter = CancelJobs { guild_id: Some(guild_id.to_string()), job_type: Some("captcha.timeout".into()), ..Default::default() };
            match self.app.scheduler().try_cancel_jobs(filter).await {
                Ok(n) if n > 0 => notices.push(format!("Cancelled {n} scheduled kick{} of unverified members.", if n == 1 { "" } else { "s" })),
                Ok(_) => {}
                Err(e) => warn("cancelling captcha kicks", &e),
            }
        }
        if (changes.contains_key("captchaTemplate") || changes.contains_key("captchaVerifyButton"))
            && let Some(d) = &discord
            && let Some(n) = refresh_captcha_panel(guild_id, &written, d.as_ref()).await
        {
            notices.push(n);
        }
        if BIRTHDAY_SCHEDULE_FIELDS.iter().any(|f| changed(f)) {
            feat_achievements::birthday_service::ensure_tick_job(&self.app, guild_id, Some(&written), true).await;
        }
        let num = |v: &Value, f: &str| v.get(f).and_then(Value::as_f64);
        if changed("economyMaxBalance")
            && let (Some(now), Some(was)) = (num(&row, "economyMaxBalance"), num(&before, "economyMaxBalance"))
            && now < was
        {
            match feat_economy::economy_service::clamp_balances_to_cap(&self.app, guild_id, i64::from(written.economy_max_balance)).await {
                Ok(n) if n > 0 => notices.push(format!("Lowered {n} balance{} to the new maximum.", if n == 1 { "" } else { "s" })),
                Ok(_) => {}
                Err(e) => warn("lowering balances", &e),
            }
        }
        if changed("modLogChannelId")
            && let Some(ch) = written.mod_log_channel_id.as_deref().filter(|c| !c.is_empty())
            && let Some(d) = &discord
        {
            let embed = serde_json::to_value(bot_core::embed_builder::success_embed("Moderation Log", Some("Moderation cases will be posted here."), None))
                .unwrap_or(Value::Null);
            let body = json!({ "embeds": [embed], "allowed_mentions": { "parse": [] } });
            if d.send_message(ch, body).await.is_err() {
                notices.push("Saved, but the bot cannot post in the moderation log channel: check its permissions there.".to_string());
            }
        }
        Ok(SettingsWritten { row, notices })
    }
}

/// Edits the posted verification panel after its message or button changed (what /captcha message does). The answer
/// is a notice for the admin, or `None` when the panel was edited.
pub async fn refresh_captcha_panel(guild_id: &str, row: &GuildSettings, discord: &dyn DashboardDiscord) -> Option<String> {
    let (Some(channel_id), Some(message_id)) =
        (row.captcha_channel_id.as_deref().filter(|c| !c.is_empty()), row.captcha_message_id.as_deref().filter(|m| !m.is_empty()))
    else {
        return Some("Saved. No verification panel is posted yet: post it with /captcha setup in Discord.".into());
    };
    let resources = match discord.get_guild_resources(guild_id, false).await {
        Ok(r) => r,
        Err(e) => return Some(panel_failure(e.discord_code)),
    };
    let Some(ch) = resources.channel(channel_id) else {
        return Some("Saved, but the verification channel was deleted: run /captcha setup in Discord.".into());
    };
    let ctx = panel_render_context(&resources, &ch.id);
    let payload = feat_captcha::captcha_service::build_panel_payload(Some(row), &ctx).to_json();
    let body = json!({
        "content": payload.get("content").cloned().unwrap_or(json!("")),
        "embeds": payload.get("embeds").cloned().unwrap_or(json!([])),
        "components": payload.get("components").cloned().unwrap_or(json!([])),
        "allowed_mentions": { "parse": [] },
    });
    match discord.edit_message(channel_id, message_id, body).await {
        Ok(_) => None,
        Err(e) => Some(panel_failure(e.discord_code)),
    }
}

/// `{ guild: guildLike(guild), channel: { id, name, type } }`: the stand-in for the discord.js Guild the panel's
/// placeholders read. Unlike the embeds store's context, `guildLike` writes the icon as
/// `${CDN}/icons/{id}/{icon}.png?size=256` for every icon, animated ones included.
pub fn panel_render_context(resources: &DiscordResources, channel_id: &str) -> bot_util::placeholder_parser::RenderContext {
    let mut ctx = render_context_for(Some(resources), Some(channel_id), None, None);
    if let Some(guild) = ctx.guild.as_mut() {
        let g = &resources.guild;
        guild.icon_url = g.icon.as_deref().filter(|i| !i.is_empty()).map(|icon| format!("{CDN}/icons/{}/{icon}.png?size=256", g.id));
    }
    ctx
}

/// `CDN` of settingsBridge.js.
const CDN: &str = "https://cdn.discordapp.com";

fn panel_failure(code: Option<i64>) -> String {
    match code {
        Some(10008) => "Saved, but the posted panel was deleted: run /captcha setup in Discord to post it again.".into(),
        Some(50001 | 50013) => "Saved, but the bot cannot edit the posted panel: check its permissions in the verification channel.".into(),
        _ => "Saved, but the posted panel could not be updated right now. Saving again retries.".into(),
    }
}

#[async_trait]
impl SettingsBridge for DashboardSettings {
    async fn read_guild_settings(&self, guild_id: &str) -> anyhow::Result<Value> {
        Ok(normalize_row(&*self.read_row(guild_id).await?))
    }

    async fn write_settings(&self, guild_id: &str, changes: Map<String, Value>, ctx: SettingsWriteCtx) -> Result<SettingsWritten, SettingsWriteError> {
        self.write(guild_id, changes, ctx.discord).await
    }
}
