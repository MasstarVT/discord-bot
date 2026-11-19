//! Port of `src/utils/roleTemplates.js`: reaction-role templates that create their roles. Plain role sets (pronouns,
//! colors, age groups, ...) and the timezone sets of `TIMEZONE_ROLE_TEMPLATES`, whose roles are also linked to their
//! zones for `/timestamp`. Used by `/reactionrole create template:<key>`, the dashboard's create-only Template field
//! and the `rr.roleTemplate` task; [`crate::role_template_setup`] creates them.

use bot_util::timezones::{TIMEZONE_PANEL, TIMEZONE_ROLE_TEMPLATES};

/// One role of a template. `name` is the Discord role (reused by name, ignoring case); `label` (≤ 80) and
/// `description` (≤ 100, dropdowns only) are the panel option; `emoji` is one standard emoji or `None`; `color` is a
/// new role's color (0 = none); `zone` is set on timezone templates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRole {
    pub name: &'static str,
    pub label: &'static str,
    pub emoji: Option<&'static str>,
    pub color: u32,
    pub description: Option<&'static str>,
    pub zone: Option<&'static str>,
}

/// The panel a template posts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplatePanel {
    /// `NORMAL` or `UNIQUE`.
    pub kind: &'static str,
    /// `BUTTONS` or `SELECT`.
    pub layout: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub placeholder: Option<&'static str>,
}

/// One role template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTemplate {
    pub key: &'static str,
    pub label: &'static str,
    pub panel: TemplatePanel,
    pub roles: Vec<TemplateRole>,
    pub note: Option<&'static str>,
    pub links_zones: bool,
}

const fn role(name: &'static str, emoji: Option<&'static str>) -> TemplateRole {
    TemplateRole { name, label: name, emoji, color: 0, description: None, zone: None }
}

const fn colored(name: &'static str, emoji: &'static str, color: u32) -> TemplateRole {
    TemplateRole { name, label: name, emoji: Some(emoji), color, description: None, zone: None }
}

const fn panel(kind: &'static str, layout: &'static str, title: &'static str, description: &'static str, placeholder: &'static str) -> TemplatePanel {
    TemplatePanel { kind, layout, title, description, placeholder: Some(placeholder) }
}

/// The note of every timezone template.
pub const TIMEZONE_NOTE: &str = "Each role is linked to its timezone, so `/timestamp` reads a member's times in the zone they pick; a member's own `/timezone set` still wins over the role.";

fn role_sets() -> Vec<RoleTemplate> {
    // Mid-tone colors, readable on both the dark and the light Discord theme (so no black or white).
    vec![
        RoleTemplate {
            key: "pronouns",
            label: "Pronouns",
            panel: panel("NORMAL", "BUTTONS", "💬 Pronoun Roles", "Pick the pronouns you go by. You can hold more than one.", "Pick your pronouns"),
            roles: vec![
                role("He/Him", Some("🩵")),
                role("She/Her", Some("🩷")),
                role("They/Them", Some("💜")),
                role("Any Pronouns", Some("🤍")),
                role("Ask My Pronouns", Some("💬")),
            ],
            note: None,
            links_zones: false,
        },
        RoleTemplate {
            key: "colors",
            label: "Colors",
            panel: panel(
                "UNIQUE",
                "SELECT",
                "🎨 Color Roles",
                "Pick a color for your name. You can hold one color at a time; pick another to switch.",
                "Pick a color",
            ),
            roles: vec![
                colored("Red", "❤️", 0xEF4444),
                colored("Orange", "🧡", 0xF97316),
                colored("Yellow", "💛", 0xEAB308),
                colored("Green", "💚", 0x22C55E),
                colored("Light Blue", "🩵", 0x38BDF8),
                colored("Blue", "💙", 0x3B82F6),
                colored("Purple", "💜", 0xA855F7),
                colored("Pink", "🩷", 0xEC4899),
                colored("Brown", "🤎", 0xB45309),
                colored("Grey", "🩶", 0x9CA3AF),
            ],
            note: Some(
                "A color only shows when its role sits above a member's other colored roles, so drag the color roles up in **Server Settings → Roles** (they stay below my highest role).",
            ),
            links_zones: false,
        },
        RoleTemplate {
            key: "age",
            label: "Age groups",
            panel: panel("UNIQUE", "BUTTONS", "🎂 Age Roles", "Pick your age group. You can hold one at a time.", "Pick your age group"),
            // Discord's terms set the minimum age at 13, so there is no younger group.
            roles: vec![role("13-17", None), role("18-24", None), role("25-34", None), role("35-44", None), role("45+", None)],
            note: None,
            links_zones: false,
        },
        RoleTemplate {
            key: "regions",
            label: "Regions",
            panel: panel("UNIQUE", "BUTTONS", "🗺️ Region Roles", "Pick the part of the world you live in.", "Pick your region"),
            roles: vec![
                role("North America", Some("🌎")),
                role("South America", Some("🌴")),
                role("Europe", Some("🇪🇺")),
                role("Africa", Some("🌍")),
                role("Middle East", Some("🏜️")),
                role("Asia", Some("🌏")),
                role("Oceania", Some("🦘")),
            ],
            note: None,
            links_zones: false,
        },
        RoleTemplate {
            key: "platforms",
            label: "Gaming platforms",
            panel: panel("NORMAL", "BUTTONS", "🎮 Platform Roles", "Pick the platforms you play on. You can hold more than one.", "Pick your platforms"),
            roles: vec![
                role("PC", Some("🖥️")),
                role("PlayStation", Some("🎮")),
                role("Xbox", Some("🟢")),
                role("Nintendo Switch", Some("🔴")),
                role("Mobile", Some("📱")),
            ],
            note: None,
            links_zones: false,
        },
        RoleTemplate {
            key: "dms",
            label: "DM status",
            panel: panel("UNIQUE", "BUTTONS", "✉️ DM Roles", "Let others know whether you are open to direct messages.", "Pick your DM status"),
            roles: vec![role("DMs Open", Some("✅")), role("Ask to DM", Some("💬")), role("DMs Closed", Some("⛔"))],
            note: None,
            links_zones: false,
        },
        RoleTemplate {
            key: "pings",
            label: "Ping roles",
            panel: panel("NORMAL", "BUTTONS", "🔔 Ping Roles", "Pick what you want to be pinged for. You can hold as many as you like.", "Pick your pings"),
            roles: vec![
                role("Announcements", Some("📢")),
                role("Events", Some("📅")),
                role("Giveaways", Some("🎁")),
                role("Polls", Some("📊")),
                role("Streams", Some("🎥")),
            ],
            note: Some("The roles are created not mentionable, so only members allowed to mention @everyone can ping them."),
            links_zones: false,
        },
    ]
}

/// `ROLE_TEMPLATES`: every role template, the role sets first, then the timezone sets (always a Unique panel), in
/// the JS key order.
pub fn role_templates() -> Vec<RoleTemplate> {
    let mut out = role_sets();
    for t in TIMEZONE_ROLE_TEMPLATES.iter() {
        out.push(RoleTemplate {
            key: t.key,
            label: t.label,
            panel: TemplatePanel {
                kind: TIMEZONE_PANEL.kind,
                layout: TIMEZONE_PANEL.layout,
                title: TIMEZONE_PANEL.title,
                description: TIMEZONE_PANEL.description,
                placeholder: Some(TIMEZONE_PANEL.placeholder),
            },
            roles: t
                .roles
                .iter()
                .map(|r| TemplateRole { name: r.name, label: r.label, emoji: None, color: 0, description: Some(r.description), zone: Some(r.zone) })
                .collect(),
            note: Some(TIMEZONE_NOTE),
            links_zones: true,
        });
    }
    out
}

/// `getRoleTemplate(key)`: the template of that key, or `None`.
pub fn get_role_template(key: &str) -> Option<RoleTemplate> {
    role_templates().into_iter().find(|t| t.key == key)
}

/// The template keys, in order (`Object.keys(ROLE_TEMPLATES)`).
pub fn role_template_keys() -> Vec<&'static str> {
    role_templates().iter().map(|t| t.key).collect()
}
