//! Port of `src/dashboard/collections/index.js`: the registry of per-server record collections the dashboard manages,
//! the browser copy of a collection (`publicCollection`), request validation (`validateRecordInput`) and the
//! descriptor lint (`lintCollections`).
//!
//! The descriptors themselves are data: `collections_data.json` is generated from the JS definitions by
//! rust/fixtures/gen/dash-collections-data.mjs (function hooks left out); the hooks (`check`, an action's `check`,
//! `refKeys`, `refKeyOf`) are restated in Rust in [`crate::collections`] and attached here by key.

use std::sync::{Arc, LazyLock};

use dash_schema::js::js_keys;
use dash_schema::{Field, FieldError, Refs, RoleRef, lint_descriptors, public_descriptor, ref_key, validate_value};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};

use crate::errors::RecordError;
use crate::store::StoreError;
use crate::util::{slice16, truthy};

/// `COLLECTION_KEY_RE` (`/^[A-Za-z]{1,32}$/`).
pub fn is_collection_key(key: &str) -> bool {
    (1..=32).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphabetic())
}

/// `RECORD_ID_RE` (`/^[A-Za-z0-9_-]{1,40}$/`).
pub fn is_record_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What a request does (`mode` of validateRecordInput).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Create,
    Update,
    Action,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Action => "action",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(Self::Create),
            "update" => Some(Self::Update),
            "action" => Some(Self::Action),
            _ => None,
        }
    }
}

/// A rule a collection's `check` found broken: `{ path, message }` (path '' when it is about the whole record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

impl Problem {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { path: path.into(), message: message.into() }
    }

    /// `asFieldError(problem)`.
    pub fn into_field_error(self) -> FieldError {
        FieldError::new(self.path, self.message)
    }

    /// `{ path, message }`.
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "path": self.path, "message": self.message })
    }
}

/// The second argument of a check (`{ mode, current }`), plus the clock the JS reads with `Date.now()`.
#[derive(Debug, Clone, Copy)]
pub struct CheckCtx<'a> {
    pub mode: Mode,
    /// The stored item (update and action), as the store's `get` answered it.
    pub current: Option<&'a Map<String, Value>>,
    /// Epoch ms ("now").
    pub now_ms: f64,
}

/// A collection's (or an action's) cross-field rule: None, or the first problem.
pub type CheckFn = fn(&Map<String, Value>, &CheckCtx) -> Option<Problem>;
/// `collection.refKeys(item, keys)`: refines the stored ref keys of an item.
pub type RefKeysFn = fn(Option<&Value>, IndexSet<String>) -> IndexSet<String>;
/// `collection.refKeyOf(ref, data)`: the key of a role in a request.
pub type RefKeyOfFn = fn(&RoleRef, &Map<String, Value>) -> String;

/// One action of a collection (`{ id, label, description, danger, confirm, showWhen, fields, check? }`).
#[derive(Debug, Clone)]
pub struct CollectionAction {
    raw: Map<String, Value>,
    pub id: String,
    pub fields: Vec<Field>,
    pub check: Option<CheckFn>,
}

impl CollectionAction {
    fn from_map(raw: Map<String, Value>) -> Self {
        let id = raw.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let fields = fields_of(raw.get("fields"));
        Self { raw, id, fields, check: None }
    }

    /// The action's JSON object (without its check).
    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }

    /// The browser copy (the `actions` entries of publicCollection).
    pub fn public_json(&self) -> Value {
        let r = &self.raw;
        let mut out = Map::new();
        copy(r, &mut out, "id");
        copy(r, &mut out, "label");
        out.insert("description".into(), nullish_or(r.get("description"), Value::String(String::new())));
        out.insert("danger".into(), Value::Bool(r.get("danger") == Some(&Value::Bool(true))));
        out.insert("confirm".into(), nullish_or(r.get("confirm"), Value::Null));
        out.insert("showWhen".into(), nullish_or(r.get("showWhen"), Value::Null));
        out.insert("fields".into(), Value::Array(self.fields.iter().map(public_descriptor).collect()));
        Value::Object(out)
    }
}

/// One collection definition (`src/dashboard/collections/<key>.js` `collection`).
#[derive(Debug, Clone)]
pub struct Collection {
    raw: Map<String, Value>,
    pub key: String,
    pub fields: Vec<Field>,
    pub read_only: Vec<Field>,
    pub create_only: Vec<String>,
    pub actions: Vec<CollectionAction>,
    pub check: Option<CheckFn>,
    pub ref_keys: Option<RefKeysFn>,
    pub ref_key_of: Option<RefKeyOfFn>,
}

fn fields_of(v: Option<&Value>) -> Vec<Field> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|d| d.as_object().cloned().map(Field::from_map)).collect())
        .unwrap_or_default()
}

fn copy(from: &Map<String, Value>, to: &mut Map<String, Value>, key: &str) {
    if let Some(v) = from.get(key) {
        to.insert(key.into(), v.clone());
    }
}

fn nullish_or(v: Option<&Value>, fallback: Value) -> Value {
    match v {
        Some(v) if !v.is_null() => v.clone(),
        _ => fallback,
    }
}

impl Collection {
    /// A collection from its JSON definition (key order as in the JS source). No hooks: add them with the `with_*`
    /// methods. `Err` when `v` is not an object.
    pub fn from_json(v: Value) -> Result<Self, String> {
        let Value::Object(raw) = v else { return Err("a collection must be an object".into()) };
        let key = raw.get("key").and_then(Value::as_str).unwrap_or_default().to_string();
        let fields = fields_of(raw.get("fields"));
        let read_only = fields_of(raw.get("readOnly"));
        let create_only = raw
            .get("createOnly")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| dash_schema::js::to_js_string(Some(v))).collect())
            .unwrap_or_default();
        let actions = raw
            .get("actions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_object().cloned().map(CollectionAction::from_map)).collect())
            .unwrap_or_default();
        Ok(Self { raw, key, fields, read_only, create_only, actions, check: None, ref_keys: None, ref_key_of: None })
    }

    /// With a `check`.
    pub fn with_check(mut self, check: CheckFn) -> Self {
        self.check = Some(check);
        self
    }

    /// With an action's `check`.
    pub fn with_action_check(mut self, action_id: &str, check: CheckFn) -> Self {
        if let Some(a) = self.actions.iter_mut().find(|a| a.id == action_id) {
            a.check = Some(check);
        }
        self
    }

    /// With `refKeys` and `refKeyOf`.
    pub fn with_ref_keys(mut self, ref_keys: RefKeysFn, ref_key_of: RefKeyOfFn) -> Self {
        self.ref_keys = Some(ref_keys);
        self.ref_key_of = Some(ref_key_of);
        self
    }

    /// The definition's JSON object (without its hooks).
    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }

    /// One key of the definition.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    fn str_key(&self, key: &str) -> Option<&str> {
        self.raw.get(key).and_then(Value::as_str)
    }

    /// `c.module`.
    pub fn module(&self) -> &str {
        self.str_key("module").unwrap_or_default()
    }

    /// `c.label`.
    pub fn label(&self) -> &str {
        self.str_key("label").unwrap_or_default()
    }

    /// `c.singular` (the server's "That {singular} does not exist any more.").
    pub fn singular(&self) -> &str {
        self.str_key("singular").unwrap_or_default()
    }

    /// `c.limit` (null: no "n / limit" count).
    pub fn limit(&self) -> Value {
        self.raw.get("limit").cloned().unwrap_or(Value::Null)
    }

    /// `c.titleField`.
    pub fn title_field(&self) -> Option<&str> {
        self.str_key("titleField")
    }

    /// `c.checkStoredRoles === true`: any update re-checks every assignable role of the saved record.
    pub fn check_stored_roles(&self) -> bool {
        self.raw.get("checkStoredRoles") == Some(&Value::Bool(true))
    }

    /// `c.placeholder` (truthy).
    pub fn placeholder(&self) -> bool {
        truthy(self.raw.get("placeholder"))
    }

    /// `c.searchable === true`.
    pub fn searchable(&self) -> bool {
        self.raw.get("searchable") == Some(&Value::Bool(true))
    }

    /// The action with this id.
    pub fn action(&self, id: &str) -> Option<&CollectionAction> {
        self.actions.iter().find(|a| a.id == id)
    }

    /// `[...c.fields, ...(c.readOnly ?? [])]`.
    pub fn all_fields(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().chain(self.read_only.iter())
    }

    /// `publicCollection(c)`: the collection as the browser gets it (server-only hooks removed, descriptors made
    /// JSON-safe), keys in the JS order.
    pub fn public_json(&self) -> Value {
        let r = &self.raw;
        let mut out = Map::new();
        copy(r, &mut out, "key");
        copy(r, &mut out, "module");
        copy(r, &mut out, "label");
        copy(r, &mut out, "singular");
        out.insert("description".into(), nullish_or(r.get("description"), Value::String(String::new())));
        copy(r, &mut out, "limit");
        copy(r, &mut out, "titleField");
        out.insert("subtitleField".into(), nullish_or(r.get("subtitleField"), Value::Null));
        out.insert("avatarField".into(), nullish_or(r.get("avatarField"), Value::Null));
        for (k, def) in [("canCreate", true), ("canEdit", true), ("canDelete", true)] {
            let _ = def;
            out.insert(k.into(), Value::Bool(r.get(k) != Some(&Value::Bool(false))));
        }
        out.insert("searchable".into(), Value::Bool(self.searchable()));
        out.insert("searchPlaceholder".into(), nullish_or(r.get("searchPlaceholder"), Value::Null));
        out.insert("fields".into(), Value::Array(self.fields.iter().map(public_descriptor).collect()));
        out.insert("createOnly".into(), Value::Array(self.create_only.iter().cloned().map(Value::String).collect()));
        out.insert("readOnly".into(), Value::Array(self.read_only.iter().map(public_descriptor).collect()));
        out.insert("actions".into(), Value::Array(self.actions.iter().map(CollectionAction::public_json).collect()));
        Value::Object(out)
    }

    /// The stored ref keys of an item, refined by `refKeys` when the collection has it (server.js `recordRefs`'
    /// `keys`).
    pub fn ref_keys_of(&self, item: Option<&Value>, keys: IndexSet<String>) -> IndexSet<String> {
        match self.ref_keys {
            Some(f) => f(item, keys),
            None => keys,
        }
    }

    /// The key a role of a request is kept under (`collection.refKeyOf(ref, data)`, default `refKey(path, id)`).
    pub fn ref_key_for(&self, r: &RoleRef, data: &Map<String, Value>) -> String {
        match self.ref_key_of {
            Some(f) => f(r, data),
            None => ref_key(&r.path, &r.id),
        }
    }

    /// Every channel and role id an item holds, and which field holds it (server.js `recordRefs`).
    pub fn record_refs(&self, item: Option<&Value>) -> (dash_schema::StoredRefs, IndexSet<String>) {
        let mut refs = dash_schema::StoredRefs::default();
        let mut keys = IndexSet::new();
        let obj = item.and_then(Value::as_object);
        for desc in self.all_fields() {
            let raw = obj.and_then(|o| o.get(desc.field()));
            dash_schema::collect_stored_refs(desc, raw, &mut refs);
            dash_schema::stored_ref_keys(desc, raw, Some(desc.field()), &mut keys);
        }
        (refs, self.ref_keys_of(item, keys))
    }
}

/// What [`Collections::validate_record_input`] answers: the data to hand the store, and the channel and role ids it
/// refers to (to check against the server).
#[derive(Debug, Clone, PartialEq)]
pub struct ValidInput {
    pub data: Map<String, Value>,
    pub refs: Refs,
}

/// An ordered set of collections (`COLLECTION_LIST` / `COLLECTIONS`). [`Collections::builtin`] is the real one; tests
/// build their own (server.js takes `deps.collections`).
#[derive(Debug, Clone)]
pub struct Collections {
    list: Arc<IndexMap<String, Arc<Collection>>>,
}

impl Collections {
    /// A set in display order (a later duplicate key replaces the earlier one).
    pub fn new(list: Vec<Collection>) -> Self {
        Self { list: Arc::new(list.into_iter().map(|c| (c.key.clone(), Arc::new(c))).collect()) }
    }

    /// The 22 collections of the dashboard, in display order.
    pub fn builtin() -> &'static Collections {
        &BUILTIN
    }

    /// `COLLECTIONS[key]` (own keys only).
    pub fn get(&self, key: &str) -> Option<&Arc<Collection>> {
        self.list.get(key)
    }

    /// `COLLECTION_LIST`.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Collection>> {
        self.list.values()
    }

    /// The keys in display order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.list.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// `validateRecordInput(key, data, { mode, actionId, current })`: validates the `data` of a create, update or action
    /// request against the collection's descriptors.
    /// - create: every field is validated; a missing field takes its default (or fails when `required`);
    /// - update: only the fields present are validated; createOnly and unknown fields are refused;
    /// - action: the fields of that action are validated like a create.
    ///
    /// Then the collection's `check` (or the action's) runs. Errors: `StoreError::Field` (with the path) or
    /// `StoreError::Record` (not_found: unknown list or action).
    pub fn validate_record_input(
        &self,
        key: &str,
        data: &Value,
        mode: Mode,
        action_id: Option<&str>,
        current: Option<&Value>,
        now_ms: f64,
    ) -> Result<ValidInput, StoreError> {
        let c = self.get(key).ok_or_else(|| RecordError::not_found("Unknown list."))?;
        let Some(data) = data.as_object() else {
            return Err(FieldError::new("", "\"data\" must be an object.").into());
        };
        let action = if mode == Mode::Action {
            let found = action_id.and_then(|id| c.action(id));
            Some(found.ok_or_else(|| RecordError::not_found("Unknown action."))?)
        } else {
            None
        };
        let fields: &[Field] = action.map_or(&c.fields, |a| &a.fields);

        for k in js_keys(data) {
            let known = fields.iter().find(|f| f.field() == k);
            let forbidden = matches!(k.as_str(), "__proto__" | "constructor" | "prototype");
            let Some(f) = known.filter(|_| !forbidden) else {
                let short = slice16(k, 40);
                return Err(FieldError::new(short, format!("\"{short}\" is not something you can set here.")).into());
            };
            if mode == Mode::Update && c.create_only.iter().any(|x| x == k) {
                let label = dash_schema::js::to_js_string(f.get("label"));
                return Err(FieldError::new(k.as_str(), format!("\"{label}\" cannot be changed after creation.")).into());
            }
        }
        let mut refs = Refs::default();
        let mut out = Map::new();
        for f in fields {
            let name = f.field();
            if let Some(v) = data.get(name) {
                out.insert(name.to_string(), validate_value(f, v, Some(name), &mut refs)?);
            } else if mode != Mode::Update {
                if f.required() {
                    let label = dash_schema::js::to_js_string(f.get("label"));
                    return Err(FieldError::new(name, format!("\"{label}\" is required.")).into());
                }
                out.insert(name.to_string(), f.default_or_null());
            }
        }
        let check = match action {
            Some(a) => a.check,
            None => c.check,
        };
        if let Some(check) = check {
            let current = current.and_then(Value::as_object);
            let ctx = CheckCtx { mode, current, now_ms };
            if let Some(problem) = check(&out, &ctx) {
                return Err(problem.into_field_error().into());
            }
        }
        Ok(ValidInput { data: out, refs })
    }

    /// `lintCollections()`: descriptor problems of every collection (empty = fine).
    pub fn lint(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for c in self.iter() {
            if !is_collection_key(&c.key) {
                problems.push(format!("{}: bad key", c.key));
            }
            problems.extend(lint_descriptors(&c.fields, &format!("{}.", c.key)));
            problems.extend(
                lint_descriptors(&c.read_only, &format!("{}(readOnly).", c.key)).into_iter().filter(|p| !p.contains("the default")),
            );
            for a in &c.actions {
                problems.extend(lint_descriptors(&a.fields, &format!("{}!{}.", c.key, a.id)));
            }
            for f in &c.create_only {
                if !c.fields.iter().any(|d| d.field() == f) {
                    problems.push(format!("{}: createOnly {f} is not a field", c.key));
                }
            }
            let title = c.get("titleField");
            let titled = c.all_fields().any(|d| d.get("field").is_some_and(|f| title.is_some_and(|t| dash_schema::js::strict_eq(f, t))));
            if !c.placeholder() && !titled && title != Some(&Value::String("id".into())) {
                problems.push(format!("{}: titleField {} is not a field", c.key, dash_schema::js::to_js_string(title)));
            }
        }
        problems
    }
}

/// `validateRecordInput` on the built-in collections.
pub fn validate_record_input(
    key: &str,
    data: &Value,
    mode: Mode,
    action_id: Option<&str>,
    current: Option<&Value>,
    now_ms: f64,
) -> Result<ValidInput, StoreError> {
    Collections::builtin().validate_record_input(key, data, mode, action_id, current, now_ms)
}

/// `lintCollections()` on the built-in collections.
pub fn lint_collections() -> Vec<String> {
    Collections::builtin().lint()
}

/// `publicCollection(c)`.
pub fn public_collection(c: &Collection) -> Value {
    c.public_json()
}

/// The embedded data file.
#[derive(serde::Deserialize)]
struct Data {
    order: Vec<String>,
    collections: Map<String, Value>,
    hooks: Map<String, Value>,
}

static DATA: LazyLock<Data> =
    LazyLock::new(|| serde_json::from_str(include_str!("collections_data.json")).expect("collections_data.json is valid"));

/// The function hooks the JS definitions have, per collection key (`check`, `actions.<id>.check`, `refKeys`, ...):
/// the generated table the tests compare with the Rust hooks.
pub fn js_hook_names() -> Vec<(String, Vec<String>)> {
    DATA.hooks
        .iter()
        .map(|(k, v)| {
            let names = v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
            (k.clone(), names)
        })
        .collect()
}

/// The hooks the Rust collection has, named like [`js_hook_names`].
pub fn rust_hook_names(c: &Collection) -> Vec<String> {
    let mut out = Vec::new();
    if c.check.is_some() {
        out.push("check".to_string());
    }
    if c.ref_keys.is_some() {
        out.push("refKeys".to_string());
    }
    if c.ref_key_of.is_some() {
        out.push("refKeyOf".to_string());
    }
    for a in &c.actions {
        if a.check.is_some() {
            out.push(format!("actions.{}.check", a.id));
        }
    }
    out
}

static BUILTIN: LazyLock<Collections> = LazyLock::new(|| {
    let list = DATA
        .order
        .iter()
        .map(|key| {
            let raw = DATA.collections.get(key).cloned().unwrap_or(Value::Null);
            let c = Collection::from_json(raw).expect("a collection definition is an object");
            crate::collections::attach_hooks(c)
        })
        .collect();
    Collections::new(list)
});
