//! Port of `src/dashboard/landing.js`: the public pages. The landing page (GET /, public/landing.html), the module and
//! command reference (/docs, public/docs.html), the changelog (/changelog, public/changelog.html) and the header and
//! footer of the legal pages (/privacy, /terms). Each template is filled with data from the dashboard catalog,
//! src/dashboard/commands.json and CHANGELOG.md. Pure: no I/O (the server reads the templates and CHANGELOG.md).
//!
//! Markers, all replaced in one pass (inserted text is never scanned again); an unknown marker is an error:
//! - every page: `<!--module-count-->` `<!--command-count-->` `<!--contact-->` `<!--support-->` `<!--code:KEY-->`
//!   `<!--flavour:KEY-->` `@@INVITE_URL@@`
//! - landing: `<!--module-table-->` `<!--bot-facts-->` `<!--ingredients-->` `<!--latest-changes-->` `<!--levels-demo-->`
//! - docs: `<!--docs-toc-->` `<!--docs-modules-->` `<!--docs-core-->`
//! - changelog: `<!--changelog-intro-->` `<!--changelog-->`

use std::collections::HashMap;
use std::sync::LazyLock;

use bot_util::jsre::JsRegex;
use dash_auth::changelog::{LATEST_COUNT, parse_changelog, render_changelog_entries, render_latest_changes};
use dash_auth::command_list::CommandSummary;
use dash_auth::config::Contact;
use dash_auth::docs::{command_id, module_id, render_core_commands, render_docs_modules, render_docs_toc};
use dash_auth::site_parts::{
    SUPPORT_LABEL, escape_html, flavour_of, inline_markdown, public_description, public_note, render_contact, render_support,
    scalar_settings, who_label,
};
use dash_schema::js::{strict_eq, to_js_string_or_empty};
use serde_json::Value;

/// Where "Add to Discord" points when no invite link is configured.
const FALLBACK_INVITE: &str = "/dashboard";
/// Settings a module wrapper lists before pointing at /docs for the rest.
const WRAPPER_SETTINGS: usize = 4;

static MARKERS: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"<!--([a-z]+(?:-[a-z]+)*)(?::([^>]*?))?-->|@@INVITE_URL@@", "g"));

/// src/dashboard/commands.json (the public command list).
pub static COMMANDS: LazyLock<Vec<CommandSummary>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../src/dashboard/commands.json")).expect("src/dashboard/commands.json parses")
});

/// What the pages are filled with (`{ inviteUrl, contact, supportUrl, changelog }`).
#[derive(Debug, Clone, Default)]
pub struct PageOptions {
    pub invite_url: Option<String>,
    pub contact: Option<Contact>,
    pub support_url: Option<String>,
    /// The text of CHANGELOG.md (its newest entries are listed); without it the list is empty.
    pub changelog: Option<String>,
}

/// `escapeHtml(value)` of a JSON value (`String(value ?? '')`).
fn esc(v: Option<&Value>) -> String {
    escape_html(&to_js_string_or_empty(v))
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn categories(catalog: &Value) -> &[Value] {
    catalog.get("categories").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn modules_of(category: &Value) -> Vec<&str> {
    category.get("modules").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn module<'a>(catalog: &'a Value, key: &str) -> &'a Value {
    catalog.get("modules").and_then(|m| m.get(key)).unwrap_or(&Value::Null)
}

fn module_count(catalog: &Value) -> usize {
    catalog.get("modules").and_then(Value::as_object).map_or(0, |m| m.len())
}

/// "1 module" / "4 modules".
fn count_label(n: usize) -> String {
    format!("{n} {}", if n == 1 { "module" } else { "modules" })
}

/// The "what's inside" box: per category a flavour heading and a run of wrappers.
fn render_module_table(catalog: &Value) -> String {
    categories(catalog)
        .iter()
        .map(|category| {
            let flavour = flavour_of(str_of(category, "id"));
            let keys = modules_of(category);
            let pops: Vec<String> = keys
                .iter()
                .map(|key| {
                    let m = module(catalog, key);
                    let cmds: Vec<&CommandSummary> = COMMANDS.iter().filter(|c| c.module.as_deref() == Some(*key)).collect();
                    let settings: Vec<(&Value, String)> = scalar_settings(m).into_iter().take(WRAPPER_SETTINGS).collect();
                    let on = truthy(m.get("default"));
                    let enable = if on {
                        format!("<p class=\"pop-line\">On when the bot joins. Off: <code>/module disable {}</code></p>", escape_html(key))
                    } else {
                        format!("<p class=\"pop-line\">Switch it on: <code>/module enable {}</code></p>", escape_html(key))
                    };
                    let setup = if truthy(m.get("configCommand")) {
                        format!("<p class=\"pop-line\">Set it up: <code>{}</code></p>", esc(m.get("configCommand")))
                    } else {
                        String::new()
                    };
                    let cmd_list = if cmds.is_empty() {
                        String::new()
                    } else {
                        let items: String = cmds
                            .iter()
                            .map(|c| {
                                format!(
                                    "<li><a href=\"/docs#{}\"><code>/{}</code></a> <span>{}</span></li>",
                                    escape_html(&command_id(&c.name)),
                                    escape_html(&c.name),
                                    escape_html(&public_description(&c.description))
                                )
                            })
                            .collect();
                        format!("<p class=\"pop-sub\">Commands</p><ul class=\"pop-cmds\">{items}</ul>")
                    };
                    let setting_list = if settings.is_empty() {
                        String::new()
                    } else {
                        let items: String = settings
                            .iter()
                            .map(|(field, value)| format!("<div><dt>{}</dt><dd>{}</dd></div>", esc(field.get("label")), escape_html(value)))
                            .collect();
                        format!("<p class=\"pop-sub\">Settings, as they ship</p><dl class=\"pop-settings\">{items}</dl>")
                    };
                    [
                        format!("<li class=\"pop pop-{}\">", flavour.cls),
                        "<details class=\"pop-pack\">".to_string(),
                        "<summary class=\"pop-wrapper\">".to_string(),
                        format!("<span class=\"pop-code\" aria-hidden=\"true\">{}</span>", esc(m.get("code"))),
                        "<span class=\"pop-text\">".to_string(),
                        format!("<span class=\"pop-name\">{}</span>", esc(m.get("label"))),
                        format!("<span class=\"pop-desc\">{}</span>", esc(m.get("description"))),
                        "</span>".to_string(),
                        format!("<span class=\"pop-state\">{}<span class=\"pop-state-x\"> at start</span></span>", if on { "On" } else { "Off" }),
                        "</summary>".to_string(),
                        "<div class=\"pop-inside\">".to_string(),
                        enable,
                        setup,
                        cmd_list,
                        setting_list,
                        format!(
                            "<a class=\"pop-more\" href=\"/docs#{}\">Everything in {}</a>",
                            escape_html(&module_id(key)),
                            esc(m.get("label"))
                        ),
                        "</div>".to_string(),
                        "</details>".to_string(),
                        "</li>".to_string(),
                    ]
                    .join("")
                })
                .collect();
            let id = escape_html(str_of(category, "id"));
            [
                format!("<section class=\"flavour flavour-{}\" aria-labelledby=\"flv-{id}\">", flavour.cls),
                "<div class=\"flavour-head\">".to_string(),
                format!(
                    "<h3 class=\"flavour-title\" id=\"flv-{id}\"><span class=\"flavour-name\">{}</span> {}</h3>",
                    escape_html(flavour.name),
                    esc(category.get("label"))
                ),
                format!("<span class=\"flavour-count\">{}</span>", count_label(keys.len())),
                format!("<p class=\"flavour-desc\">{}</p>", esc(category.get("description"))),
                "</div>".to_string(),
                format!("<ul class=\"pop-run\">{}</ul>", pops.join("")),
                "</section>".to_string(),
            ]
            .join("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Bot Facts panel: module counts per category and command counts per role.
fn render_bot_facts(catalog: &Value) -> String {
    let row = |label: &str, value: &str, cls: &str| format!("<div class=\"facts-row{cls}\"><dt>{label}</dt><dd>{value}</dd></div>");
    let by_who = |who: &str| COMMANDS.iter().filter(|c| c.who == who).count();
    let mut parts = vec!["<dl class=\"facts-list\">".to_string(), row("Modules", &module_count(catalog).to_string(), " facts-major")];
    for c in categories(catalog) {
        parts.push(row(&esc(c.get("label")), &modules_of(c).len().to_string(), " facts-minor"));
    }
    parts.push(row("Slash commands", &COMMANDS.len().to_string(), " facts-major facts-rule"));
    for w in ["everyone", "moderators", "admins", "owner"] {
        let n = by_who(w);
        if n > 0 {
            let label = who_label(w).unwrap_or_default().to_lowercase();
            parts.push(row(&format!("For {}", escape_html(&label)), &n.to_string(), " facts-minor"));
        }
    }
    parts.push(row("Premium tiers", "0", " facts-major facts-rule"));
    parts.push(row("API keys to buy", "0", " facts-major"));
    parts.push(row("Web dashboard", "Included", " facts-major"));
    parts.push("</dl>".to_string());
    parts.join("")
}

/// Every slash command as one ingredients line, each linked to its entry on /docs.
fn render_ingredients() -> String {
    COMMANDS
        .iter()
        .map(|c| format!("<a href=\"/docs#{}\"><code>/{}</code></a>", escape_html(&command_id(&c.name)), escape_html(&c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The Levels settings the landing page's sample dashboard form shows, straight from the catalog.
fn render_levels_demo(catalog: &Value) -> Result<String, String> {
    let mut fields: HashMap<String, &Value> = HashMap::new();
    let groups = module(catalog, "leveling").get("groups").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    for g in groups {
        for f in g.get("fields").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            fields.insert(dash_schema::js::to_js_string(f.get("field")), f);
        }
    }
    let pick = |name: &str| -> Result<&Value, String> {
        fields.get(name).copied().ok_or_else(|| format!("landing template: the Levels module has no \"{name}\" setting"))
    };
    let num = |f: &Value| -> String {
        let note = public_note(&to_js_string_or_empty(f.get("description")));
        [
            "<label class=\"ss-field\">".to_string(),
            format!("<span class=\"ss-label\">{}</span>", esc(f.get("label"))),
            format!(
                "<input class=\"ss-input\" type=\"number\" name=\"{}\" value=\"{}\" min=\"{}\" max=\"{}\" inputmode=\"numeric\">",
                esc(f.get("field")),
                esc(f.get("default")),
                esc(f.get("min")),
                esc(f.get("max"))
            ),
            if note.is_empty() { String::new() } else { format!("<span class=\"ss-note\">{}</span>", escape_html(&note)) },
            "</label>".to_string(),
        ]
        .join("")
    };
    let mode = pick("levelUpMode")?;
    let embed = pick("levelUpUseEmbed")?;
    let message = pick("levelUpMessage")?;
    let options: String = mode
        .get("options")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|o| {
            let checked = match (o.get("value"), mode.get("default")) {
                (Some(a), Some(b)) => strict_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            format!(
                "<label class=\"ss-seg-opt\"><input type=\"radio\" name=\"{}\" value=\"{}\"{}><span>{}</span></label>",
                esc(mode.get("field")),
                esc(o.get("value")),
                if checked { " checked" } else { "" },
                esc(o.get("label"))
            )
        })
        .collect();
    let placeholders: Vec<String> = message
        .get("placeholders")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|p| format!("<code>{}</code>", esc(Some(p)))).collect())
        .unwrap_or_default();
    Ok([
        format!("<div class=\"ss-pair\">{}{}</div>", num(pick("xpMin")?), num(pick("xpMax")?)),
        "<p class=\"ss-error\" id=\"ss-xp-error\" hidden>The maximum must be at least the minimum.</p>".to_string(),
        "<fieldset class=\"ss-seg\">".to_string(),
        format!("<legend class=\"ss-label\">{}</legend>", esc(mode.get("label"))),
        options,
        "</fieldset>".to_string(),
        "<label class=\"ss-toggle\">".to_string(),
        format!(
            "<input type=\"checkbox\" role=\"switch\" name=\"{}\"{}>",
            esc(embed.get("field")),
            if truthy(embed.get("default")) { " checked" } else { "" }
        ),
        format!(
            "<span class=\"ss-toggle-text\"><span class=\"ss-label\">{}</span><span class=\"ss-note\">{}</span></span>",
            esc(embed.get("label")),
            escape_html(&public_note(&to_js_string_or_empty(embed.get("description"))))
        ),
        "</label>".to_string(),
        "<label class=\"ss-field\">".to_string(),
        format!("<span class=\"ss-label\">{}</span>", esc(message.get("label"))),
        format!(
            "<textarea class=\"ss-input ss-text\" name=\"{}\" rows=\"2\" maxlength=\"{}\">{}</textarea>",
            esc(message.get("field")),
            esc(message.get("maxLength")),
            esc(message.get("default"))
        ),
        format!("<span class=\"ss-note\">Placeholders: {}</span>", placeholders.join(" ")),
        "</label>".to_string(),
    ]
    .join(""))
}

type Marker<'a> = Box<dyn Fn() -> Result<String, String> + 'a>;

/// Fills a template's markers (`fill`).
fn fill(template: &str, page: &[(&str, Marker<'_>)], options: &PageOptions, catalog: &Value) -> Result<String, String> {
    let invite = escape_html(match options.invite_url.as_deref().map(bot_util::js::trim) {
        Some(u) if !u.is_empty() => u,
        _ => FALLBACK_INVITE,
    });
    let mut category_of: HashMap<&str, &str> = HashMap::new();
    for c in categories(catalog) {
        for k in modules_of(c) {
            category_of.insert(k, str_of(c, "id"));
        }
    }
    let has_module = |key: &str| catalog.get("modules").and_then(Value::as_object).is_some_and(|m| m.contains_key(key));
    let module_arg = |name: &str, key: &str| -> Result<(), String> {
        if has_module(key) { Ok(()) } else { Err(format!("page template: unknown module \"{key}\" in a {name} marker")) }
    };
    let shared = |name: &str| -> Option<String> {
        match name {
            "module-count" => Some(module_count(catalog).to_string()),
            "command-count" => Some(COMMANDS.len().to_string()),
            "contact" => Some(render_contact(options.contact.as_ref())),
            "support" => Some(render_support(options.support_url.as_deref(), SUPPORT_LABEL)),
            _ => None,
        }
    };

    let text: Vec<u16> = template.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(text.len());
    let mut last = 0;
    for m in MARKERS.find_all(&text) {
        out.extend_from_slice(&text[last..m.start]);
        last = m.end;
        let whole = m.text(&text);
        let replacement = if whole == "@@INVITE_URL@@" {
            invite.clone()
        } else {
            let name = m.group(&text, 1).unwrap_or_default();
            let arg = m.group(&text, 2);
            match (name.as_str(), arg) {
                ("code", Some(key)) => {
                    module_arg("code", &key)?;
                    esc(module(catalog, &key).get("code"))
                }
                ("flavour", Some(key)) => {
                    module_arg("flavour", &key)?;
                    flavour_of(category_of.get(key.as_str()).copied().unwrap_or_default()).cls.to_string()
                }
                (_, Some(_)) => return Err(format!("page template: unknown marker \"{whole}\"")),
                (name, None) => match page.iter().find(|(n, _)| *n == name) {
                    Some((_, handler)) => handler()?,
                    None => shared(name).ok_or_else(|| format!("page template: unknown marker \"{whole}\""))?,
                },
            }
        };
        out.extend(replacement.encode_utf16());
    }
    out.extend_from_slice(&text[last..]);
    Ok(String::from_utf16_lossy(&out))
}

fn catalog() -> &'static Value {
    dash_collections::build_catalog()
}

/// Renders the landing page (`renderLanding`).
pub fn render_landing(template: &str, options: &PageOptions) -> Result<String, String> {
    let catalog = catalog();
    let entries = parse_changelog(options.changelog.as_deref().unwrap_or("")).entries;
    let page: Vec<(&str, Marker<'_>)> = vec![
        ("module-table", Box::new(|| Ok(render_module_table(catalog)))),
        ("bot-facts", Box::new(|| Ok(render_bot_facts(catalog)))),
        ("ingredients", Box::new(|| Ok(render_ingredients()))),
        ("latest-changes", Box::new(|| Ok(render_latest_changes(&entries, LATEST_COUNT)))),
        ("levels-demo", Box::new(|| render_levels_demo(catalog))),
    ];
    fill(template, &page, options, catalog)
}

/// Renders the module and command reference (`renderDocs`).
pub fn render_docs(template: &str, options: &PageOptions) -> Result<String, String> {
    let catalog = catalog();
    let page: Vec<(&str, Marker<'_>)> = vec![
        ("docs-toc", Box::new(|| Ok(render_docs_toc(catalog)))),
        ("docs-modules", Box::new(|| Ok(render_docs_modules(catalog, &COMMANDS)))),
        ("docs-core", Box::new(|| Ok(render_core_commands(&COMMANDS)))),
    ];
    fill(template, &page, options, catalog)
}

/// Renders a legal page's header and footer (`renderLegal`); its own `<!--contact-->` markers are already replaced.
pub fn render_legal(template: &str, options: &PageOptions) -> Result<String, String> {
    fill(template, &[], options, catalog())
}

/// Renders the changelog page (`renderChangelog`).
pub fn render_changelog(template: &str, options: &PageOptions) -> Result<String, String> {
    let catalog = catalog();
    let log = parse_changelog(options.changelog.as_deref().unwrap_or(""));
    let page: Vec<(&str, Marker<'_>)> = vec![
        ("changelog-intro", Box::new(|| Ok(inline_markdown(&log.intro)))),
        ("changelog", Box::new(|| Ok(render_changelog_entries(&log.entries)))),
    ];
    fill(template, &page, options, catalog)
}
