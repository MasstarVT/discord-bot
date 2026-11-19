//! The Discord-facing utilities against `rust/fixtures/golden/bot-core/utils.json`, produced from the JS by
//! `rust/fixtures/gen/bot-core-utils.mjs`: embed factories, permission names, customEmbed (normalise, validate,
//! render, modal build and parse, JSON import), the paginator, the confirm prompt, autocomplete lists,
//! escapeMarkdown, newTargets, and discord.js permission / role / channel / CDN results on the same guild.

use bot_core::custom_embed as ce;
use bot_core::interaction_helpers as ih;
use bot_core::paginator::{FooterMode, Pages, row_json, start_index};
use bot_core::permissions as perms;
use bot_core::privacy_guards as pv;
use bot_core::testing::{json_diff, load_golden, offline_discord_as};
use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser, RenderContext};
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, Guild, GuildId, Permissions, RoleId, UserId};

fn golden() -> Value {
    load_golden("bot-core/utils.json")
}

/// Drops `timestamp` and `type` (serenity adds `"type": "rich"`), and null values, recursively.
fn clean(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                if k == "timestamp" || k == "type" && x.is_string() || x.is_null() {
                    continue;
                }
                out.insert(k.clone(), clean(x));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(clean).collect()),
        other => other.clone(),
    }
}

fn same(label: &str, ours: &Value, theirs: &Value) {
    if let Some(d) = json_diff(&clean(ours), &clean(theirs), false) {
        panic!("{label}: {d}\nours:   {ours}\ntheirs: {theirs}");
    }
}

fn opt_str(v: &Value) -> Option<&str> {
    v.as_str()
}

#[test]
fn embed_factories_match_the_js() {
    use bot_core::embed_builder::*;
    for case in golden()["embeds"].as_array().cloned().unwrap_or_default() {
        let a = case["args"].as_array().cloned().unwrap_or_default();
        let s = |i: usize| a.get(i).and_then(Value::as_str);
        let e = match case["fn"].as_str().unwrap_or("") {
            "success" => success_embed(s(0).unwrap_or(""), s(1), s(2)),
            "error" => error_embed(s(0).unwrap_or(""), s(1), s(2)),
            "info" => info_embed(s(0).unwrap_or(""), s(1), s(2)),
            "warn" => warn_embed(s(0).unwrap_or(""), s(1), s(2)),
            _ => neutral_embed(s(0), s(1), s(2)),
        };
        same(&format!("{}{:?}", case["fn"], a.iter().map(|x| x.to_string().chars().take(20).collect::<String>()).collect::<Vec<_>>()), &serde_json::to_value(&e).unwrap_or_default(), &case["json"]);
    }
    for case in golden()["guildEmbeds"].as_array().cloned().unwrap_or_default() {
        let settings = case["settings"].as_object().map(|o| {
            let mut fields = Map::new();
            fields.insert("embedColor".into(), o.get("embedColor").cloned().unwrap_or(Value::Null));
            bot_core::settings::MemorySettingsStore::new_row("1", fields).expect("row")
        });
        let a = case["args"].as_array().cloned().unwrap_or_default();
        let e = guild_embed(settings.as_ref(), a.first().and_then(Value::as_str), a.get(1).and_then(Value::as_str), a.get(2).and_then(Value::as_str));
        same(&format!("guildEmbed {}", case["settings"]), &serde_json::to_value(&e).unwrap_or_default(), &case["json"]);
    }
}

#[test]
fn permission_names_match_the_js() {
    let g = golden();
    for c in g["permissionNames"].as_array().cloned().unwrap_or_default() {
        let keys: Vec<String> = serde_json::from_value(c["keys"].clone()).unwrap_or_default();
        assert_eq!(json!(perms::permission_names(&keys)), c["names"], "{keys:?}");
    }
    for c in g["permissionNamesOfBits"].as_array().cloned().unwrap_or_default() {
        let bits: u64 = c["bits"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        assert_eq!(json!(perms::permission_names_of_bits(Permissions::from_bits_retain(bits))), c["names"], "{bits}");
    }
    assert_eq!(json!(ih::DANGEROUS_PERMISSIONS), g["dangerous"]);
}

#[test]
fn colours_emoji_and_urls_match_the_js() {
    let g = golden();
    for c in g["colors"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(ce::parse_color(&c["input"])), c["parsed"], "parseColor({})", c["input"]);
    }
    for c in g["formatColor"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(ce::format_color(&c["n"])), c["s"], "formatColor({})", c["n"]);
    }
    for c in g["emoji"].as_array().cloned().unwrap_or_default() {
        let input = c["input"].as_str().unwrap_or_default();
        let ours = ce::parse_button_emoji(input);
        // discord.js resolves `{ name }` with `animated: false`; compare on id, name and animated when present.
        let norm = |v: &Value| -> Value {
            if v.is_null() {
                return Value::Null;
            }
            json!({ "id": v.get("id"), "name": v.get("name"), "animated": v.get("animated").cloned().unwrap_or(json!(false)) })
        };
        assert_eq!(norm(&ours.clone().unwrap_or(Value::Null)), norm(&c["parsed"]), "parseButtonEmoji({input:?})");
    }
    for c in g["httpUrl"].as_array().cloned().unwrap_or_default() {
        let u = c["u"].as_str().unwrap_or_default();
        assert_eq!(ce::is_http_url(u), c["ok"].as_bool().unwrap_or(false), "isHttpUrl({u:?})");
    }
}

fn render_ctx(g: &Value) -> RenderContext {
    let rc = &g["renderContext"];
    let u = &rc["userA"];
    let gu = &rc["guildA"];
    let user = PlaceholderUser {
        id: u["id"].as_str().unwrap_or_default().into(),
        username: u["username"].as_str().map(String::from),
        tag: u["tag"].as_str().map(String::from),
        discriminator: u["discriminator"].as_str().map(String::from),
        global_name: u["globalName"].as_str().map(String::from),
        bot: false,
        created_timestamp: u["createdTimestamp"].as_f64(),
        avatar_url: u["displayAvatarURL"].as_str().map(String::from),
    };
    let guild = PlaceholderGuild {
        id: gu["id"].as_str().unwrap_or_default().into(),
        name: gu["name"].as_str().map(String::from),
        owner_id: gu["ownerId"].as_str().map(String::from),
        member_count: gu["memberCount"].as_f64(),
        verification_level: Some(gu["verificationLevel"].to_string()),
        joined_timestamp: gu["joinedTimestamp"].as_f64(),
        created_timestamp: gu["createdTimestamp"].as_f64(),
        premium_subscription_count: Some(gu["premiumSubscriptionCount"].to_string()),
        premium_tier: Some(gu["premiumTier"].to_string()),
        icon_url: gu["iconURL"].as_str().map(String::from),
    };
    let m = &rc["memberA"];
    RenderContext {
        member: Some(PlaceholderMember {
            display_name: m["displayName"].as_str().map(String::from),
            joined_timestamp: m["joinedTimestamp"].as_f64(),
            avatar_url: m["avatarURL"].as_str().map(String::from),
            user: Some(user),
            guild: Some(guild),
        }),
        channel: Some(PlaceholderChannel { id: "500".into(), name: Some("general".into()), kind: Some(0) }),
        ..Default::default()
    }
}

#[test]
fn templates_normalise_validate_and_render_like_the_js() {
    let g = golden();
    let ctx = render_ctx(&g);
    for (i, c) in g["templates"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        let input = &c["input"];
        let label = format!("template #{i}");
        let normalized = ce::normalize_template(input);
        assert_eq!(serde_json::to_string(&normalized).unwrap_or_default(), serde_json::to_string(&c["normalized"]).unwrap_or_default(), "{label}: normalizeTemplate (key order too)");
        assert_eq!(json!(ce::template_to_json(input)), c["json"], "{label}: templateToJson");
        let v = ce::validate_template(input, true);
        assert_eq!(json!({ "ok": v.ok, "errors": v.errors }), c["validate"], "{label}: validateTemplate");
        let v = ce::validate_template(input, false);
        assert_eq!(json!({ "ok": v.ok, "errors": v.errors }), c["validateLoose"], "{label}: validateTemplate loose");
        let rendered = ce::render_template(input, &ctx, ce::RenderOptions::default());
        match (&rendered, c["rendered"].is_null()) {
            (None, true) => {}
            (Some(r), false) => {
                let theirs = &c["rendered"];
                assert_eq!(json!(r.content), theirs.get("content").cloned().unwrap_or(Value::Null), "{label}: content");
                same(&format!("{label}: embeds"), &json!(r.embeds), &theirs["embeds"]);
                same(&format!("{label}: components"), &json!(r.link_rows), &theirs["components"]);
                assert_eq!(serde_json::to_value(&r.allowed_mentions).ok().map(|v| v["parse"].clone()), Some(json!(["users"])), "{label}: allowedMentions");
            }
            (ours, _) => panic!("{label}: rendered {} vs {}", ours.is_some(), c["rendered"]),
        }
        let embed = normalized.get("embed").filter(|e| e.is_object()).cloned().unwrap_or(json!({}));
        let no_color = ce::render_embed(&embed, &ctx, None);
        same(&format!("{label}: renderEmbed(defaultColor null)"), &json!(no_color), &c["renderedNoColor"]);
        let modal = ce::template_modal_json("tpl:edit", Some("Edit the welcome message please now"), input, false).expect("clamp mode never fails");
        same(&format!("{label}: modal"), &modal, &c["modal"]);
        let thrown = ce::template_modal_json("tpl:edit", None, input, true).err().map(|e| json!({ "message": e.to_string(), "fields": e.fields }));
        assert_eq!(thrown.unwrap_or(Value::Null), c["modalThrow"], "{label}: modal overflow");
    }
}

#[test]
fn template_modal_parsing_matches_the_js() {
    for c in golden()["modalParse"].as_array().cloned().unwrap_or_default() {
        let fields = c["fields"].clone();
        let base = if c["base"].is_null() { ce::empty_template() } else { c["base"].clone() };
        let (tpl, errors) = ce::parse_template_modal(|id| fields.get(id).and_then(Value::as_str).map(String::from), &base, c["requireSomething"].as_bool().unwrap_or(true));
        assert_eq!(serde_json::to_string(&tpl).unwrap_or_default(), serde_json::to_string(&c["template"]).unwrap_or_default(), "template for {fields}");
        assert_eq!(json!(errors), c["errors"], "errors for {fields}");
    }
}

#[test]
fn template_import_matches_the_js() {
    let long = format!("{{\"content\":\"{}\"}}", "a".repeat(21000));
    let inputs = ["{\"content\":\"hi\"}", "{\"embeds\":[{\"title\":\"T\"}]}", "{}", "\"just text\"", long.as_str(), "{\"embed\":{\"title\":\"t\",\"url\":\"nope\"}}"];
    let cases = golden()["fromJson"].as_array().cloned().unwrap_or_default();
    assert_eq!(cases.len(), inputs.len());
    for (s, c) in inputs.iter().zip(cases) {
        let (tpl, errors) = ce::template_from_json(s);
        assert_eq!(json!(tpl), c["template"], "{}", c["s"]);
        assert_eq!(json!(errors), c["errors"], "{}", c["s"]);
    }
    let (none, errors) = ce::template_from_json("{nope");
    assert!(none.is_none() && errors[0].starts_with("That is not valid JSON: "));
}

#[test]
fn paginator_pages_and_buttons_match_the_js() {
    for c in golden()["paginator"].as_array().cloned().unwrap_or_default() {
        let footers = c["footers"].as_array().cloned().unwrap_or_default();
        let pages: Vec<Value> = footers
            .iter()
            .enumerate()
            .map(|(i, f)| if f.is_null() { json!({ "title": format!("P{i}") }) } else { json!({ "title": format!("P{i}"), "footer": f }) })
            .collect();
        let mode = match c["opts"]["footer"].as_str() {
            Some("replace") => FooterMode::Replace,
            Some("none") => FooterMode::None,
            _ => FooterMode::Append,
        };
        let n = pages.len();
        let p = Pages::from_json(pages, mode);
        let starts: Vec<usize> = match c["opts"]["startPage"].as_f64() {
            Some(s) => vec![start_index(s, n)],
            None => (0..n).collect(),
        };
        for (r, i) in c["rendered"].as_array().cloned().unwrap_or_default().iter().zip(starts) {
            same(&format!("page {i} of {footers:?}"), &p.render_json(i), &r["embed"]);
            let rows = if n > 1 { json!([row_json(i, n, false)]) } else { json!([]) };
            assert_eq!(rows, r["components"], "buttons of page {i}");
        }
    }
}

#[test]
fn confirm_prompt_matches_the_js() {
    let cases = golden()["confirm"].as_array().cloned().unwrap_or_default();
    let opts = [
        ih::ConfirmOptions { title: Some("Delete it?".into()), description: Some("Really".into()), ..Default::default() },
        ih::ConfirmOptions { danger: false, confirm_label: "Yes please".into(), cancel_label: "No".into(), ..Default::default() },
    ];
    for (c, o) in cases.iter().zip(opts) {
        let nonce = c["components"][0]["components"][0]["custom_id"].as_str().unwrap_or("").split(':').nth(1).unwrap_or("").to_string();
        assert_eq!(nonce.len(), 12);
        let (embed, row) = ih::confirm_prompt(&o, &nonce);
        let mut theirs = c["embed"].clone();
        theirs.as_object_mut().map(|m| m.remove("footer"));
        same("confirm embed", &serde_json::to_value(&embed).unwrap_or_default(), &theirs);
        let ours = serde_json::to_value(&row).unwrap_or_default();
        for (a, b) in ours["components"].as_array().into_iter().flatten().zip(c["components"][0]["components"].as_array().into_iter().flatten()) {
            assert_eq!((&a["custom_id"], &a["label"], &a["style"]), (&b["custom_id"], &b["label"], &b["style"]));
        }
        assert_eq!(c["ephemeral"], json!(true));
    }
}

#[test]
fn autocomplete_lists_match_the_js() {
    for c in golden()["autocomplete"].as_array().cloned().unwrap_or_default() {
        let choices: Vec<ih::Choice> =
            c["input"].as_array().into_iter().flatten().map(|x| ih::Choice { name: x.get("name").and_then(Value::as_str).map(String::from), value: x["value"].clone() }).collect();
        let ours: Vec<Value> = ih::autocomplete_list(&choices).into_iter().map(|(n, v)| json!({ "name": n, "value": v })).collect();
        assert_eq!(json!(ours), c["output"]);
    }
}

#[test]
fn escape_markdown_matches_the_js() {
    for c in golden()["markdown"].as_array().cloned().unwrap_or_default() {
        let s = c["s"].as_str().unwrap_or_default();
        assert_eq!(json!(bot_core::markdown::escape_markdown(s)), c["e"], "escapeMarkdown({s:?})");
    }
}

#[test]
fn new_targets_pure_helpers_match_the_js() {
    use bot_core::new_targets as nt;
    let g = golden();
    let n = &g["newTargets"];
    for c in n["channelTypeFor"].as_array().cloned().unwrap_or_default() {
        let t: Vec<u8> = serde_json::from_value(c["t"].clone()).unwrap_or_default();
        assert_eq!(json!(nt::channel_type_for(&t)), c["r"], "{t:?}");
    }
    for c in n["findByName"].as_array().cloned().unwrap_or_default() {
        let names: Vec<String> = serde_json::from_value(c["names"].clone()).unwrap_or_default();
        let r = nt::find_by_name(names.iter(), c["name"].as_str().unwrap_or(""), c["slugged"].as_bool().unwrap_or(false), |s| s.as_str());
        assert_eq!(json!(r), c["r"], "{c}");
    }
    let payload = feat_payload();
    let defs = nt::leaf_definitions(&payload, None, Some("set"));
    let pairs: Vec<Value> = nt::companion_pairs(&defs).into_iter().map(|p| json!({ "newName": p.new_name, "base": p.base })).collect();
    assert_eq!(json!(pairs), n["companionPairs"]);
    assert_eq!(nt::new_role_option("role").to_json(), n["roleOption"]);
    assert_eq!(nt::new_channel_option("category", "category").to_json(), n["channelOption"]);
    assert_eq!(nt::reuse_note_for(&[ChannelId::new(5), ChannelId::new(6)]).as_deref(), Some("ℹ️ Reused the existing <#5>, <#6> instead of creating a new private channel."));
    assert_eq!(nt::describe_create_error(&nt::CreateError::Discord(Some(30005)), "role"), "This server already has the maximum number of roles (250). Delete one or pick an existing role.");
}

/// `/counting`'s payload from the golden file (the newTargets helpers read the command payload).
fn feat_payload() -> Value {
    load_golden("commands.json").as_array().and_then(|a| a.iter().find(|c| c["name"] == "counting").map(|c| c["payload"].clone())).unwrap_or_default()
}

fn the_guild(g: &Value) -> (Guild, UserId) {
    let d = offline_discord_as(&[g["guild"].clone()]);
    let id = GuildId::new(g["guild"]["id"].as_str().and_then(|s| s.parse().ok()).unwrap_or(1));
    let guild = d.cache.guild(id).map(|x| x.clone()).expect("cached");
    (guild, UserId::new(g["botId"].as_str().and_then(|s| s.parse().ok()).unwrap_or(1)))
}

fn id(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[test]
fn permissions_roles_and_channels_match_discord_js() {
    let g = golden();
    let (guild, bot) = the_guild(&g);
    let channel = |v: &Value| perms::find_channel(&guild, ChannelId::new(id(v))).cloned().expect("channel");
    let member = |v: &Value| guild.members.get(&UserId::new(id(v))).cloned().expect("member");
    for c in g["channelPerms"].as_array().cloned().unwrap_or_default() {
        let (ch, m) = (channel(&c["channel"]), member(&c["member"]));
        let ours = perms::member_permissions_in(&guild, &ch, m.user.id, &m.roles, true).map(|p| p.bits().to_string()).unwrap_or_else(|| "null".into());
        assert_eq!(json!(ours), c["bits"], "permissionsFor {} in {}", c["member"], c["channel"]);
    }
    for c in g["rolePerms"].as_array().cloned().unwrap_or_default() {
        let ch = channel(&c["channel"]);
        let role = guild.roles.get(&RoleId::new(id(&c["role"]))).cloned().expect("role");
        let ours = perms::role_permissions_in(&guild, &ch, &role, true).map(|p| p.bits().to_string()).unwrap_or_else(|| "null".into());
        assert_eq!(json!(ours), c["bits"], "permissionsFor role {} in {}", c["role"], c["channel"]);
    }
    for c in g["guildPerms"].as_array().cloned().unwrap_or_default() {
        let m = member(&c["member"]);
        assert_eq!(json!(perms::guild_permissions(&guild, m.user.id, &m.roles).bits().to_string()), c["bits"]);
    }
    for c in g["positions"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(perms::role_position(&guild, RoleId::new(id(&c["role"])))), c["position"], "position of {}", c["role"]);
    }
    for c in g["highest"].as_array().cloned().unwrap_or_default() {
        let m = member(&c["member"]);
        assert_eq!(json!(perms::highest_position(&guild, &m.roles)), c["position"], "highest of {}", c["member"]);
    }
    for c in g["everyoneCanView"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(pv::everyone_can_view(&guild, Some(&channel(&c["channel"])))), c["r"], "everyoneCanView {}", c["channel"]);
    }
    for c in g["copyLeaks"].as_array().cloned().unwrap_or_default() {
        let (a, b) = (channel(&c["source"]), channel(&c["target"]));
        assert_eq!(json!(pv::copy_leaks_private(&guild, Some(&a), Some(&b))), c["r"], "copyLeaksPrivate {} -> {}", c["source"], c["target"]);
    }
    for c in g["memberChannelProblem"].as_array().cloned().unwrap_or_default() {
        let (ch, m) = (channel(&c["channel"]), member(&c["member"]));
        let needs: Vec<String> = serde_json::from_value(c["needs"].clone()).unwrap_or_else(|_| pv::MEMBER_CHANNEL_NEEDS.iter().map(|s| s.to_string()).collect());
        let needs: Vec<&str> = needs.iter().map(String::as_str).collect();
        assert_eq!(json!(pv::member_channel_problem(&guild, Some(&ch), Some(pv::MemberRef::of(&m)), &needs)), c["r"], "memberChannelProblem {c}");
    }
    for c in g["roleHierarchy"].as_array().cloned().unwrap_or_default() {
        let m = member(&c["member"]);
        let ids: Vec<String> = serde_json::from_value(c["ids"].clone()).unwrap_or_default();
        assert_eq!(json!(pv::role_hierarchy_problem(&guild, Some(pv::MemberRef::of(&m)), &ids)), c["r"], "roleHierarchyProblem {c}");
    }
    for c in g["assignable"].as_array().cloned().unwrap_or_default() {
        let role = if c["role"].is_null() { None } else { guild.roles.get(&RoleId::new(id(&c["role"]))).cloned() };
        let ex = if c["executor"].is_null() { None } else { Some(member(&c["executor"])) };
        let r = ih::check_assignable_role(&guild, role.as_ref(), bot, ex.as_ref(), c["allowDangerous"].as_bool().unwrap_or(false));
        let mut ours = json!({ "ok": r.ok, "dangerous": r.dangerous });
        if let Some(reason) = r.reason {
            ours["reason"] = json!(reason);
        }
        assert_eq!(ours, c["r"], "checkAssignableRole {} by {}", c["role"], c["executor"]);
    }
    for c in g["pingable"].as_array().cloned().unwrap_or_default() {
        let ch = channel(&c["channel"]);
        let ex = if c["executor"].is_null() { None } else { Some(member(&c["executor"])) };
        let r = ih::pingable_role_ids(&guild, &ch, ex.as_ref().map(|m| (m.user.id, m.roles.as_slice())), &["800000000000000003", "800000000000000002", "900000000000000100", "42", "800000000000000003"]);
        assert_eq!(json!({ "allowed": r.allowed, "refused": r.refused }), c["r"], "pingableRoleIds {c}");
    }
    let me = guild.members.get(&bot).cloned().expect("bot member");
    for c in g["usable"].as_array().cloned().unwrap_or_default() {
        let ch = channel(&c["channel"]);
        let needs: Vec<String> = serde_json::from_value(c["needs"].clone()).unwrap_or_else(|_| ih::CHANNEL_POST_NEEDS.iter().map(|s| s.to_string()).collect());
        let needs: Vec<&str> = needs.iter().map(String::as_str).collect();
        let bits = perms::member_permissions_in(&guild, &ch, bot, &me.roles, true);
        let r = ih::channel_usable_verdict(Some(&ch), bits, &needs);
        let mut ours = json!({ "ok": r.ok, "missing": r.missing });
        if let Some(reason) = r.reason {
            ours["reason"] = json!(reason);
        }
        assert_eq!(ours, c["r"], "checkChannelUsable {c}");
    }
    let bits = perms::guild_permissions(&guild, bot, &me.roles);
    assert_eq!(json!(ih::missing_from(Some(bits), &["ManageRoles", "BanMembers", "ViewChannel", "ManageGuild"])), g["missingGuild"]);
}

#[test]
fn cdn_urls_and_names_match_discord_js() {
    let g = golden();
    let (guild, _) = the_guild(&g);
    for c in g["avatars"].as_array().cloned().unwrap_or_default() {
        let m = guild.members.get(&UserId::new(id(&c["member"]))).cloned().expect("member");
        assert_eq!(json!(bot_core::adapters::user_display_avatar_url(&m.user)), c["user"], "user avatar {}", c["member"]);
        assert_eq!(json!(bot_core::adapters::member_display_avatar_url(guild.id, &m)), c["member_url"], "member avatar {}", c["member"]);
        assert_eq!(json!(bot_core::adapters::member_display_name(&m)), c["displayName"]);
        assert_eq!(json!(bot_core::adapters::user_tag(&m.user)), c["tag"]);
        assert_eq!(json!(bot_core::adapters::discriminator(&m.user)), c["discriminator"]);
    }
    assert_eq!(json!(bot_core::adapters::guild_icon_url(guild.id, guild.icon.as_ref())), g["guildIcon"]);
    let pg = bot_core::adapters::placeholder_guild(&guild);
    assert_eq!(pg.verification_level.as_deref(), Some("2"));
    assert_eq!(pg.premium_tier.as_deref(), Some("2"));
    assert_eq!(pg.premium_subscription_count.as_deref(), Some("7"));
    assert_eq!(pg.member_count, Some(6.0));
    let _ = opt_str(&Value::Null);
}
