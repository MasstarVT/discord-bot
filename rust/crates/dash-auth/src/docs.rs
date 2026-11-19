//! Port of `src/dashboard/docs.js`: the public command and module reference (/docs): per catalog category, each
//! module's switch, setup command, settings with their defaults and its slash commands; then the core commands.
//! The catalog is taken as the JSON `buildCatalog()` serialises to (`categories`, `modules`), so this crate does not
//! depend on the catalog crate.

use serde_json::Value;

use crate::command_list::CommandSummary;
use crate::site_parts::{escape_html, flavour_of, js_string, public_description, public_note, scalar_settings, who_label};

/// The anchor of a module on /docs.
pub fn module_id(key: &str) -> String {
    format!("mod-{key}")
}

/// The anchor of a command on /docs.
pub fn command_id(name: &str) -> String {
    format!("cmd-{name}")
}

/// `escapeHtml`'s `String(value ?? '')` of a property: absent or null reads as ''.
fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_string(x),
    }
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// One command: name, who may run it, description and subcommands.
fn render_command(cmd: &CommandSummary) -> String {
    let subs = if cmd.subcommands.is_empty() {
        String::new()
    } else {
        let items: String = cmd
            .subcommands
            .iter()
            .map(|sub| {
                format!(
                    "<li><code>/{} {}</code> {}</li>",
                    escape_html(&cmd.name),
                    escape_html(&sub.name),
                    escape_html(&public_description(&sub.description))
                )
            })
            .collect();
        format!("<ul class=\"leaf-subs\">{items}</ul>")
    };
    [
        format!("<li class=\"leaf-cmd\" id=\"{}\">", escape_html(&command_id(&cmd.name))),
        "<div class=\"leaf-cmd-head\">".to_string(),
        format!("<code class=\"leaf-cmd-name\">/{}</code>", escape_html(&cmd.name)),
        format!("<span class=\"leaf-who\">{}</span>", escape_html(who_label(&cmd.who).unwrap_or(&cmd.who))),
        "</div>".to_string(),
        format!("<p class=\"leaf-cmd-desc\">{}</p>", escape_html(&public_description(&cmd.description))),
        subs,
        "</li>".to_string(),
    ]
    .concat()
}

fn module_keys(category: &Value) -> Vec<String> {
    arr(category, "modules").iter().map(js_string).collect()
}

fn module<'a>(catalog: &'a Value, key: &str) -> &'a Value {
    catalog.get("modules").and_then(|m| m.get(key)).unwrap_or(&Value::Null)
}

/// The sidebar contents list: getting started, each category with its modules, core commands, help.
pub fn render_docs_toc(catalog: &Value) -> String {
    let cats: String = arr(catalog, "categories")
        .iter()
        .map(|c| {
            let id = s(c, "id");
            let mods: String = module_keys(c)
                .iter()
                .map(|k| {
                    format!(
                        "<li><a href=\"#{}\">{}</a></li>",
                        escape_html(&module_id(k)),
                        escape_html(&s(module(catalog, k), "label"))
                    )
                })
                .collect();
            [
                format!(
                    "<li class=\"toc-cat toc-{}\"><a href=\"#cat-{}\">{}</a>",
                    flavour_of(&id).cls,
                    escape_html(&id),
                    escape_html(&s(c, "label"))
                ),
                format!("<ul>{mods}</ul>"),
                "</li>".to_string(),
            ]
            .concat()
        })
        .collect();
    format!(
        "<li><a href=\"#start\">Getting started</a></li><li><a href=\"#who\">Who can run what</a></li>{cats}<li><a href=\"#core\">Core commands</a></li><li><a href=\"#help\">Help and support</a></li>"
    )
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

/// Every module, grouped by category.
pub fn render_docs_modules(catalog: &Value, commands: &[CommandSummary]) -> String {
    arr(catalog, "categories")
        .iter()
        .map(|c| {
            let id = s(c, "id");
            let flavour = flavour_of(&id);
            let mods: Vec<String> = module_keys(c)
                .iter()
                .map(|key| {
                    let m = module(catalog, key);
                    let cmds: Vec<&CommandSummary> =
                        commands.iter().filter(|cmd| cmd.module.as_deref() == Some(key.as_str())).collect();
                    let settings = scalar_settings(m);
                    let switch_line = if truthy(m.get("default")) {
                        format!("On when the bot joins. Turn it off with <code>/module disable {}</code>.", escape_html(key))
                    } else {
                        format!(
                            "Off until you run <code>/module enable {}</code> or flip its switch on the dashboard.",
                            escape_html(key)
                        )
                    };
                    let setup = if truthy(m.get("configCommand")) {
                        format!(" Set it up with <code>{}</code>.", escape_html(&s(m, "configCommand")))
                    } else {
                        String::new()
                    };
                    let table = if settings.is_empty() {
                        "<p class=\"leaf-note\">Configured with its commands and the lists on the dashboard.</p>".to_string()
                    } else {
                        let rows: String = settings
                            .iter()
                            .map(|(field, value)| {
                                let desc = field.get("description").map(js_string).unwrap_or_default();
                                format!(
                                    "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td></tr>",
                                    escape_html(&s(field, "label")),
                                    escape_html(value),
                                    escape_html(&public_note(&desc))
                                )
                            })
                            .collect();
                        [
                            "<table class=\"leaf-settings\"><caption>Settings and their defaults</caption>",
                            "<thead><tr><th scope=\"col\">Setting</th><th scope=\"col\">Default</th><th scope=\"col\">What it does</th></tr></thead><tbody>",
                            &rows,
                            "</tbody></table>",
                        ]
                        .concat()
                    };
                    let mid = escape_html(&module_id(key));
                    let cmd_list = if cmds.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "<ul class=\"leaf-cmds\">{}</ul>",
                            cmds.iter().map(|c| render_command(c)).collect::<String>()
                        )
                    };
                    [
                        format!("<section class=\"leaf-mod\" id=\"{mid}\" aria-labelledby=\"{mid}-h\">"),
                        format!(
                            "<h3 class=\"leaf-mod-title\" id=\"{mid}-h\"><span class=\"leaf-code\">{}</span>{}</h3>",
                            escape_html(&s(m, "code")),
                            escape_html(&s(m, "label"))
                        ),
                        format!("<p class=\"leaf-mod-desc\">{}</p>", escape_html(&s(m, "description"))),
                        format!("<p class=\"leaf-switch\">{switch_line}{setup}</p>"),
                        table,
                        cmd_list,
                        "</section>".to_string(),
                    ]
                    .concat()
                })
                .collect();
            let cid = escape_html(&id);
            [
                format!(
                    "<section class=\"leaf-cat leaf-{}\" id=\"cat-{cid}\" aria-labelledby=\"cat-{cid}-h\">",
                    flavour.cls
                ),
                format!("<h2 class=\"leaf-cat-title\" id=\"cat-{cid}-h\">{}</h2>", escape_html(&s(c, "label"))),
                format!("<p class=\"leaf-cat-desc\">{}</p>", escape_html(&s(c, "description"))),
                mods.join("\n"),
                "</section>".to_string(),
            ]
            .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The commands that belong to no module (always on).
pub fn render_core_commands(commands: &[CommandSummary]) -> String {
    let items: String = commands.iter().filter(|c| c.module.is_none()).map(render_command).collect();
    format!("<ul class=\"leaf-cmds\">{items}</ul>")
}
