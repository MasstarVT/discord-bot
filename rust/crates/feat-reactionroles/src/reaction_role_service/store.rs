//! The DB access of `reactionRoleService.js` (the command and the dashboard call only these).

use bot_db::query::delete_where;
use bot_db::{
    DbError, InsertBuilder, PgPool, REACTION_ROLE_GROUP_INFO, REACTION_ROLE_ITEM_INFO, ReactionRoleGroup, ReactionRoleItem, SelectBuilder,
    SqlValue, TxOptions, UpdateBuilder,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use super::pure::{DraftItem, ItemView, NewItem, PanelView, draft_item, order_items};

/// A panel row with its items in position order (`include: { items: { orderBy: { position: 'asc' } } }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    #[serde(flatten)]
    pub group: ReactionRoleGroup,
    pub items: Vec<ReactionRoleItem>,
}

impl Panel {
    /// The view the payload builders read.
    pub fn view(&self) -> PanelView {
        PanelView {
            id: self.group.id.clone(),
            title: self.group.title.clone(),
            description: self.group.description.clone(),
            kind: self.group.r#type.as_str().to_string(),
            layout: self.group.layout.as_str().to_string(),
            max_roles: self.group.max_roles,
            plain_text: self.group.plain_text,
            message_content: self.group.message_content.clone(),
            placeholder: self.group.placeholder.clone(),
            items: self.items.iter().map(ItemView::from).collect(),
        }
    }

    pub fn kind(&self) -> &'static str {
        self.group.r#type.as_str()
    }

    pub fn layout(&self) -> &'static str {
        self.group.layout.as_str()
    }

    pub fn role_ids(&self) -> Vec<String> {
        self.items.iter().map(|i| i.role_id.clone()).collect()
    }
}

async fn items_of<'e, E: sqlx::PgExecutor<'e>>(exec: E, group_id: &str) -> Result<Vec<ReactionRoleItem>, DbError> {
    SelectBuilder::new(&REACTION_ROLE_ITEM_INFO).filter("groupId", json!(group_id))?.order_by("position", true)?.build()?.fetch_all(exec).await
}

/// `getPanel(guildId, panelId)`: the panel of this guild with its items, or `None`.
pub async fn get_panel(pool: &PgPool, guild_id: &str, panel_id: &str) -> Result<Option<Panel>, DbError> {
    let group: Option<ReactionRoleGroup> = SelectBuilder::new(&REACTION_ROLE_GROUP_INFO)
        .filter("id", json!(panel_id))?
        .filter("guildId", json!(guild_id))?
        .limit(1)
        .build()?
        .fetch_optional(pool)
        .await?;
    let Some(group) = group else { return Ok(None) };
    let items = items_of(pool, &group.id).await?;
    Ok(Some(Panel { group, items }))
}

/// `countPanels(guildId)`.
pub async fn count_panels(pool: &PgPool, guild_id: &str) -> Result<i64, DbError> {
    let n: i64 = sqlx::query_scalar(COUNT_PANELS_SQL).bind(guild_id).fetch_one(pool).await?;
    Ok(n)
}

const COUNT_PANELS_SQL: &str = r#"SELECT COUNT(*)::bigint FROM "ReactionRoleGroup" WHERE "guildId" = $1"#;

/// `findPanelByMessage(guildId, messageId)`: `{ id, title }` or `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRef {
    pub id: String,
    pub title: String,
}

pub async fn find_panel_by_message(pool: &PgPool, guild_id: &str, message_id: &str) -> Result<Option<PanelRef>, DbError> {
    let row = sqlx::query(FIND_BY_MESSAGE_SQL).bind(guild_id).bind(message_id).fetch_optional(pool).await?;
    Ok(row.map(|r| PanelRef { id: r.get("id"), title: r.get("title") }))
}

const FIND_BY_MESSAGE_SQL: &str = r#"SELECT "id", "title" FROM "ReactionRoleGroup" WHERE "guildId" = $1 AND "messageId" = $2 LIMIT 1"#;

/// Prisma's `contains` as the JS bot runs it: the text wrapped in `%`, with no escaping (a `%` or `_` the admin types
/// is a wildcard there too; the cross-language test checks this against Prisma).
pub fn like_contains(q: &str) -> String {
    format!("%{q}%")
}

/// A row of [`search_panels`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelSearchRow {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub layout: String,
    pub attached: bool,
    pub item_count: i64,
}

const SEARCH_PANELS_SQL: &str = r#"SELECT g."id", g."title", g."type"::text AS "type", g."layout"::text AS "layout", g."attached",
  (SELECT COUNT(*) FROM "ReactionRoleItem" i WHERE i."groupId" = g."id")::bigint AS "itemCount"
FROM "ReactionRoleGroup" g
WHERE g."guildId" = $1 AND ($2::text IS NULL OR g."title" ILIKE $3 OR g."messageId" LIKE $3 OR g."id" = $2)
ORDER BY g."createdAt" DESC
LIMIT 25"#;

/// `searchPanels(guildId, q)`: newest first, at most 25; `q` matches the title (any case), the message id or the id.
pub async fn search_panels(pool: &PgPool, guild_id: &str, q: &str) -> Result<Vec<PanelSearchRow>, DbError> {
    let q_opt = (!q.is_empty()).then(|| q.to_string());
    let rows = sqlx::query(SEARCH_PANELS_SQL).bind(guild_id).bind(q_opt).bind(like_contains(q)).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| PanelSearchRow {
            id: r.get("id"),
            title: r.get("title"),
            kind: r.get("type"),
            layout: r.get("layout"),
            attached: r.get("attached"),
            item_count: r.get("itemCount"),
        })
        .collect())
}

/// A row of [`search_items`] (`{ id, roleId, label, emoji }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemSearchRow {
    pub id: String,
    pub role_id: String,
    pub label: String,
    pub emoji: Option<String>,
}

const SEARCH_ITEMS_SQL: &str = r#"SELECT i."id", i."roleId", i."label", i."emoji"
FROM "ReactionRoleItem" i JOIN "ReactionRoleGroup" g ON g."id" = i."groupId"
WHERE i."groupId" = $1 AND g."guildId" = $2
  AND ($3::text IS NULL OR i."label" ILIKE $4 OR i."roleId" LIKE $4 OR (cardinality($5::text[]) > 0 AND i."roleId" = ANY($5::text[])))
ORDER BY i."position" ASC
LIMIT 25"#;

/// `searchItems(guildId, panelId, q, roleIds)`: at most 25 items in position order; `q` matches the label (any case)
/// or the role id, and `role_ids` (roles whose name matches) by id.
pub async fn search_items(pool: &PgPool, guild_id: &str, panel_id: &str, q: &str, role_ids: &[String]) -> Result<Vec<ItemSearchRow>, DbError> {
    let q_opt = (!q.is_empty()).then(|| q.to_string());
    let rows = sqlx::query(SEARCH_ITEMS_SQL)
        .bind(panel_id)
        .bind(guild_id)
        .bind(q_opt)
        .bind(like_contains(q))
        .bind(role_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| ItemSearchRow { id: r.get("id"), role_id: r.get("roleId"), label: r.get("label"), emoji: r.get("emoji") })
        .collect())
}

/// A row of [`list_panels`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelListRow {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub layout: String,
    pub max_roles: Option<i32>,
    pub attached: bool,
    pub plain_text: bool,
    pub channel_id: String,
    pub message_id: String,
    #[serde(with = "bot_db::serde_js::date_opt", default)]
    pub message_missing_at: Option<chrono::DateTime<chrono::Utc>>,
    pub item_count: i64,
}

const LIST_PANELS_SQL: &str = r#"SELECT g."id", g."title", g."type"::text AS "type", g."layout"::text AS "layout", g."maxRoles", g."attached",
  g."plainText", g."channelId", g."messageId", g."messageMissingAt",
  (SELECT COUNT(*) FROM "ReactionRoleItem" i WHERE i."groupId" = g."id")::bigint AS "itemCount"
FROM "ReactionRoleGroup" g
WHERE g."guildId" = $1
ORDER BY g."createdAt" DESC"#;

/// `listPanels(guildId)`: every panel of the guild, newest first, with its item count.
pub async fn list_panels(pool: &PgPool, guild_id: &str) -> Result<Vec<PanelListRow>, DbError> {
    let rows = sqlx::query(LIST_PANELS_SQL).bind(guild_id).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let missing: Option<chrono::NaiveDateTime> = r.get("messageMissingAt");
            PanelListRow {
                id: r.get("id"),
                title: r.get("title"),
                kind: r.get("type"),
                layout: r.get("layout"),
                max_roles: r.get("maxRoles"),
                attached: r.get("attached"),
                plain_text: r.get("plainText"),
                channel_id: r.get("channelId"),
                message_id: r.get("messageId"),
                message_missing_at: missing.map(|d| d.and_utc()),
                item_count: r.get("itemCount"),
            }
        })
        .collect())
}

/// A panel to create (`createPanel(draft)`): the group fields with an explicit id, plus items built with
/// [`draft_item`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelDraft {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub title: String,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub layout: String,
    pub max_roles: Option<i32>,
    pub plain_text: bool,
    pub message_content: Option<String>,
    pub attached: bool,
    pub placeholder: Option<String>,
    pub items: Vec<DraftItem>,
}

impl PanelDraft {
    /// The view the payload builders read.
    pub fn view(&self) -> PanelView {
        PanelView {
            id: self.id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            kind: self.kind.clone(),
            layout: self.layout.clone(),
            max_roles: self.max_roles,
            plain_text: self.plain_text,
            message_content: self.message_content.clone(),
            placeholder: self.placeholder.clone(),
            items: self.items.iter().map(ItemView::from).collect(),
        }
    }
}

fn item_insert(group_id: &str, it: &DraftItem) -> Result<InsertBuilder, DbError> {
    InsertBuilder::new(&REACTION_ROLE_ITEM_INFO).values([
        ("groupId", json!(group_id)),
        ("customId", json!(it.custom_id)),
        ("roleId", json!(it.role_id)),
        ("label", json!(it.label)),
        ("emoji", json!(it.emoji)),
        ("style", json!(it.style)),
        ("position", json!(it.position)),
        ("description", json!(it.description)),
    ])
}

/// `createPanel(draft)`: the panel and its items in one transaction (the caller ensures the guild settings row).
pub async fn create_panel(pool: &PgPool, draft: &PanelDraft) -> Result<(), DbError> {
    let d = draft.clone();
    bot_db::transaction(pool, TxOptions::default(), move |tx| {
        Box::pin(async move {
            InsertBuilder::new(&REACTION_ROLE_GROUP_INFO)
                .values([
                    ("id", json!(d.id)),
                    ("guildId", json!(d.guild_id)),
                    ("channelId", json!(d.channel_id)),
                    ("messageId", json!(d.message_id)),
                    ("title", json!(d.title)),
                    ("description", json!(d.description)),
                    ("type", json!(d.kind)),
                    ("layout", json!(d.layout)),
                    ("maxRoles", json!(d.max_roles)),
                    ("plainText", json!(d.plain_text)),
                    ("messageContent", json!(d.message_content)),
                    ("attached", json!(d.attached)),
                    ("placeholder", json!(d.placeholder)),
                ])?
                .without_returning()
                .build()?
                .execute(&mut *tx)
                .await?;
            for it in &d.items {
                item_insert(&d.id, it)?.without_returning().build()?.execute(&mut *tx).await?;
            }
            Ok::<_, DbError>(())
        })
    })
    .await
}

/// `addItem(group, item)`: adds an item at the end. A duplicate role fails with the unique violation
/// (`DbError::is_unique_violation`, Prisma's P2002), unchanged.
pub async fn add_item(pool: &PgPool, group_id: &str, current: &[ReactionRoleItem], item: &NewItem) -> anyhow::Result<ReactionRoleItem> {
    let position = current.iter().map(|i| i.position).max().map(|m| m + 1).unwrap_or(0);
    let draft = draft_item(group_id, item, position)?;
    Ok(item_insert(group_id, &draft)?.build()?.fetch_one(pool).await?)
}

/// An item for [`insert_items`]: an existing one (with its id and position) or a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedItem {
    Existing { id: String, role_id: String, position: i32 },
    New(NewItem),
}

impl OrderedItem {
    fn role_id(&self) -> &str {
        match self {
            OrderedItem::Existing { role_id, .. } => role_id,
            OrderedItem::New(n) => &n.role_id,
        }
    }
}

/// `insertItems(group, items, order)`: adds `items` and sets every item's position from `order` (role ids, first to
/// last) in one transaction. A role missing from `order` keeps its relative place after the listed ones.
pub async fn insert_items(pool: &PgPool, group_id: &str, existing: &[ReactionRoleItem], items: &[NewItem], order: &[String]) -> anyhow::Result<()> {
    let mut all: Vec<OrderedItem> =
        existing.iter().map(|i| OrderedItem::Existing { id: i.id.clone(), role_id: i.role_id.clone(), position: i.position }).collect();
    all.extend(items.iter().cloned().map(OrderedItem::New));
    let all = order_items(&all, order, |it| it.role_id());
    let mut drafts = Vec::new();
    for (position, it) in all.iter().enumerate() {
        if let OrderedItem::New(n) = it {
            drafts.push((position, draft_item(group_id, n, position as i32)?));
        }
    }
    let gid = group_id.to_string();
    bot_db::transaction(pool, TxOptions::default(), move |tx| {
        Box::pin(async move {
            let mut drafts = drafts.into_iter();
            for (position, it) in all.iter().enumerate() {
                match it {
                    OrderedItem::Existing { id, position: p, .. } => {
                        if *p != position as i32 {
                            set_position(&mut *tx, id, position as i32).await?;
                        }
                    }
                    OrderedItem::New(_) => {
                        if let Some((_, d)) = drafts.next() {
                            item_insert(&gid, &d)?.without_returning().build()?.execute(&mut *tx).await?;
                        }
                    }
                }
            }
            Ok::<_, DbError>(())
        })
    })
    .await?;
    Ok(())
}

async fn set_position(tx: &mut sqlx::PgConnection, item_id: &str, position: i32) -> Result<(), DbError> {
    UpdateBuilder::new(&REACTION_ROLE_ITEM_INFO).set("position", json!(position))?.filter("id", json!(item_id))?.without_returning().build()?.execute(tx).await?;
    Ok(())
}

async fn renumber(tx: &mut sqlx::PgConnection, group_id: &str) -> Result<(), DbError> {
    let rest = items_of(&mut *tx, group_id).await?;
    for (i, it) in rest.iter().enumerate() {
        if it.position != i as i32 {
            set_position(&mut *tx, &it.id, i as i32).await?;
        }
    }
    Ok(())
}

/// `listUniquePanels(guildId)`: this guild's Unique dropdown and button panels the bot posted, with their items.
pub async fn list_unique_panels(pool: &PgPool, guild_id: &str) -> Result<Vec<Panel>, DbError> {
    let groups: Vec<ReactionRoleGroup> = bot_db::BuiltQuery {
        sql: format!(
            r#"SELECT {} FROM "ReactionRoleGroup" WHERE "guildId" = $1 AND "type" = 'UNIQUE' AND "attached" = false AND "layout" IN ('SELECT', 'BUTTONS')"#,
            REACTION_ROLE_GROUP_INFO.select_list()
        ),
        args: vec![SqlValue::Text(Some(guild_id.to_string()))],
    }
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for g in groups {
        let items = items_of(pool, &g.id).await?;
        out.push(Panel { group: g, items });
    }
    Ok(out)
}

/// `removeItem(group, itemId)`: deletes one item and renumbers the positions 0..n-1 in one transaction.
pub async fn remove_item(pool: &PgPool, group_id: &str, item_id: &str) -> Result<(), DbError> {
    let (gid, iid) = (group_id.to_string(), item_id.to_string());
    bot_db::transaction(pool, TxOptions::default(), move |tx| {
        Box::pin(async move {
            let n = delete_where(&REACTION_ROLE_ITEM_INFO, [("id", json!(iid))])?.execute(&mut *tx).await?;
            if n == 0 {
                // Prisma's delete of a missing row (P2025).
                return Err(DbError::Sqlx(sqlx::Error::RowNotFound));
            }
            renumber(&mut *tx, &gid).await
        })
    })
    .await
}

/// The fields `/reactionrole edit` and the dashboard change (`updatePanel(guildId, panelId, data)`). `Some(None)`
/// writes NULL.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelPatch {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub message_content: Option<Option<String>>,
    pub plain_text: Option<bool>,
    pub kind: Option<String>,
    pub placeholder: Option<Option<String>>,
    pub max_roles: Option<Option<i32>>,
    pub layout: Option<String>,
    pub channel_id: Option<String>,
}

impl PanelPatch {
    /// The columns this patch sets, as Prisma field names and JSON values.
    pub fn columns(&self) -> Vec<(&'static str, Value)> {
        let mut v: Vec<(&'static str, Value)> = Vec::new();
        if let Some(t) = &self.title {
            v.push(("title", json!(t)));
        }
        if let Some(d) = &self.description {
            v.push(("description", json!(d)));
        }
        if let Some(m) = &self.message_content {
            v.push(("messageContent", json!(m)));
        }
        if let Some(p) = self.plain_text {
            v.push(("plainText", json!(p)));
        }
        if let Some(k) = &self.kind {
            v.push(("type", json!(k)));
        }
        if let Some(p) = &self.placeholder {
            v.push(("placeholder", json!(p)));
        }
        if let Some(m) = &self.max_roles {
            v.push(("maxRoles", json!(m)));
        }
        if let Some(l) = &self.layout {
            v.push(("layout", json!(l)));
        }
        if let Some(c) = &self.channel_id {
            v.push(("channelId", json!(c)));
        }
        v
    }
}

/// `updatePanel(guildId, panelId, data)`: updates a panel of this guild and returns it with its items, or `None` when
/// it was deleted meanwhile.
pub async fn update_panel(pool: &PgPool, guild_id: &str, panel_id: &str, patch: &PanelPatch) -> Result<Option<Panel>, DbError> {
    let cols = patch.columns();
    if !cols.is_empty() {
        UpdateBuilder::new(&REACTION_ROLE_GROUP_INFO)
            .set_all(cols)?
            .filter("id", json!(panel_id))?
            .filter("guildId", json!(guild_id))?
            .without_returning()
            .build()?
            .execute(pool)
            .await?;
    }
    get_panel(pool, guild_id, panel_id).await
}

/// `deletePanel(guildId, panelId)`: deletes a panel of this guild (items cascade). Returns the count.
pub async fn delete_panel(pool: &PgPool, guild_id: &str, panel_id: &str) -> Result<u64, DbError> {
    delete_where(&REACTION_ROLE_GROUP_INFO, [("id", json!(panel_id)), ("guildId", json!(guild_id))])?.execute(pool).await
}

/// `movePanel(guildId, panelId, { channelId, messageId, attached })`: points a panel at a new message and clears the
/// "message deleted" marker.
pub async fn move_panel(pool: &PgPool, guild_id: &str, panel_id: &str, channel_id: &str, message_id: &str, attached: bool) -> Result<u64, DbError> {
    UpdateBuilder::new(&REACTION_ROLE_GROUP_INFO)
        .set_all([
            ("channelId", json!(channel_id)),
            ("messageId", json!(message_id)),
            ("attached", json!(attached)),
            ("messageMissingAt", Value::Null),
        ])?
        .filter("id", json!(panel_id))?
        .filter("guildId", json!(guild_id))?
        .without_returning()
        .build()?
        .execute(pool)
        .await
}

/// The flag write of `markPanelMissing`: only when it is not flagged yet (the first marker is kept).
pub async fn flag_panel_missing(pool: &PgPool, guild_id: &str, panel_id: &str) -> Result<u64, DbError> {
    UpdateBuilder::new(&REACTION_ROLE_GROUP_INFO)
        .set_typed("messageMissingAt", SqlValue::timestamp(bot_db::now_utc()))?
        .filter("id", json!(panel_id))?
        .filter("guildId", json!(guild_id))?
        .filter("messageMissingAt", Value::Null)?
        .without_returning()
        .build()?
        .execute(pool)
        .await
}

/// Clears the "message deleted" marker after a successful sync.
pub async fn clear_panel_missing(pool: &PgPool, guild_id: &str, panel_id: &str) -> Result<u64, DbError> {
    UpdateBuilder::new(&REACTION_ROLE_GROUP_INFO)
        .set("messageMissingAt", Value::Null)?
        .filter("id", json!(panel_id))?
        .filter("guildId", json!(guild_id))?
        .without_returning()
        .build()?
        .execute(pool)
        .await
}

/// `onMessageDelete`'s write: flags every panel of that message (overwriting an older flag).
pub async fn flag_message_panels_missing(pool: &PgPool, guild_id: &str, message_id: &str) -> Result<u64, DbError> {
    UpdateBuilder::new(&REACTION_ROLE_GROUP_INFO)
        .set_typed("messageMissingAt", SqlValue::timestamp(bot_db::now_utc()))?
        .filter("guildId", json!(guild_id))?
        .filter("messageId", json!(message_id))?
        .without_returning()
        .build()?
        .execute(pool)
        .await
}

/// An item of a deleted role (`{ id, groupId, emoji }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleItemRef {
    pub id: String,
    pub group_id: String,
    pub emoji: Option<String>,
}

const ITEMS_OF_ROLE_SQL: &str = r#"SELECT i."id", i."groupId", i."emoji" FROM "ReactionRoleItem" i JOIN "ReactionRoleGroup" g ON g."id" = i."groupId"
WHERE i."roleId" = $1 AND g."guildId" = $2"#;

/// The items of `role_id` across this guild's panels.
pub async fn items_of_role(pool: &PgPool, guild_id: &str, role_id: &str) -> Result<Vec<RoleItemRef>, DbError> {
    let rows = sqlx::query(ITEMS_OF_ROLE_SQL).bind(role_id).bind(guild_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| RoleItemRef { id: r.get("id"), group_id: r.get("groupId"), emoji: r.get("emoji") }).collect())
}

const DELETE_ITEMS_SQL: &str = r#"DELETE FROM "ReactionRoleItem" WHERE "id" = ANY($1::text[])"#;

/// Deletes these items and renumbers their panels, in one transaction (the prune).
pub async fn delete_items_and_renumber(pool: &PgPool, item_ids: &[String], group_ids: &[String]) -> Result<(), DbError> {
    let (ids, groups) = (item_ids.to_vec(), group_ids.to_vec());
    bot_db::transaction(pool, TxOptions::default(), move |tx| {
        Box::pin(async move {
            sqlx::query(DELETE_ITEMS_SQL).bind(ids).execute(&mut *tx).await?;
            for g in &groups {
                renumber(&mut *tx, g).await?;
            }
            Ok::<_, DbError>(())
        })
    })
    .await
}

/// The index rows (`findMany({ where: { guildId }, select: {..., items: { select: { roleId, emoji }, orderBy } } })`).
pub async fn load_index_rows(pool: &PgPool, guild_id: &str) -> Result<Vec<Panel>, DbError> {
    let groups: Vec<ReactionRoleGroup> = SelectBuilder::new(&REACTION_ROLE_GROUP_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_all(pool).await?;
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = groups.iter().map(|g| g.id.clone()).collect();
    let items: Vec<ReactionRoleItem> = bot_db::BuiltQuery {
        sql: format!(
            r#"SELECT {} FROM "ReactionRoleItem" WHERE "groupId" = ANY($1) ORDER BY "position" ASC"#,
            REACTION_ROLE_ITEM_INFO.select_list()
        ),
        args: vec![SqlValue::TextArray(ids)],
    }
    .fetch_all(pool)
    .await?;
    Ok(groups
        .into_iter()
        .map(|g| {
            let mine = items.iter().filter(|i| i.group_id == g.id).cloned().collect();
            Panel { group: g, items: mine }
        })
        .collect())
}

/// `RAW_SQL`: the raw statements of this module with sample parameters, for the DB test (each runs inside a
/// rolled-back transaction).
pub fn raw_sql() -> Vec<(&'static str, &'static str)> {
    vec![
        ("countPanels", COUNT_PANELS_SQL),
        ("findPanelByMessage", FIND_BY_MESSAGE_SQL),
        ("searchPanels", SEARCH_PANELS_SQL),
        ("searchItems", SEARCH_ITEMS_SQL),
        ("listPanels", LIST_PANELS_SQL),
        ("itemsOfRole", ITEMS_OF_ROLE_SQL),
        ("deleteItems", DELETE_ITEMS_SQL),
    ]
}
