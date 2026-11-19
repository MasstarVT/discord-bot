//! Metadata types the generated `models.rs` fills in, and the `Model` trait every model implements.

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::postgres::PgRow;

use crate::DbError;

/// The scalar type of a column (Prisma's names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarKind {
    String,
    Int,
    BigInt,
    Float,
    Boolean,
    DateTime,
    Json,
    /// A PostgreSQL enum type (its unquoted name).
    Enum(&'static str),
}

impl ScalarKind {
    /// The PostgreSQL type Prisma creates for the scalar (`information_schema.columns.udt_name`, arrays prefixed `_`).
    pub fn pg_udt_name(self) -> &'static str {
        match self {
            ScalarKind::String => "text",
            ScalarKind::Int => "int4",
            ScalarKind::BigInt => "int8",
            ScalarKind::Float => "float8",
            ScalarKind::Boolean => "bool",
            ScalarKind::DateTime => "timestamp",
            ScalarKind::Json => "jsonb",
            ScalarKind::Enum(name) => name,
        }
    }
}

/// A default the Prisma client (not the database) applies, which Rust now applies by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientDefault {
    /// No default.
    None,
    /// `@default(cuid())`: `new_cuid()`.
    Cuid,
    /// `@default(uuid())`: a v4 UUID string.
    Uuid,
    /// `@default(now())`: Prisma's query engine fills it in UTC, so Rust does too (`now_utc()`).
    Now,
    /// `@updatedAt`: set on every INSERT and UPDATE.
    UpdatedAt,
    /// A literal default or `autoincrement()`: the database applies it when the column is omitted.
    Database,
}

/// One column of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnInfo {
    /// The quoted column name in PostgreSQL.
    pub name: &'static str,
    /// The Prisma field name (the JSON key in the Redis cache). Equal to `name` unless `@map` is used.
    pub field: &'static str,
    pub kind: ScalarKind,
    /// `String[]` / `Int[]`.
    pub list: bool,
    pub nullable: bool,
    /// The DMMF `hasDefaultValue` (database or client default).
    pub has_default: bool,
    pub client_default: ClientDefault,
    /// A literal default as JSON text (`"true"`, `"0"`, `"[]"`, `"\"BUTTONS\""`), if any.
    pub default_json: Option<&'static str>,
    pub is_id: bool,
    pub is_unique: bool,
    pub updated_at: bool,
}

/// One model (table).
#[derive(Debug)]
pub struct ModelInfo {
    /// Prisma model name.
    pub name: &'static str,
    /// Table name (quoted identifier).
    pub table: &'static str,
    pub columns: &'static [ColumnInfo],
    /// Primary key columns (`@id` or `@@id`).
    pub primary_key: &'static [&'static str],
    /// Unique constraints (`@unique` fields and `@@unique` sets).
    pub unique: &'static [&'static [&'static str]],
}

impl ModelInfo {
    /// The column with this (column or field) name.
    pub fn column(&self, name: &str) -> Option<&'static ColumnInfo> {
        self.columns.iter().find(|c| c.name == name || c.field == name)
    }

    /// `"Table"` quoted for SQL.
    pub fn quoted_table(&self) -> String {
        crate::quote_ident(self.table)
    }

    /// The `@updatedAt` column, if any.
    pub fn updated_at_column(&self) -> Option<&'static ColumnInfo> {
        self.columns.iter().find(|c| c.updated_at)
    }

    /// `"a", "b", ...` for a SELECT list.
    pub fn select_list(&self) -> String {
        self.columns.iter().map(|c| crate::quote_ident(c.name)).collect::<Vec<_>>().join(", ")
    }
}

/// One PostgreSQL enum type.
#[derive(Debug)]
pub struct EnumInfo {
    pub name: &'static str,
    pub values: &'static [&'static str],
}

impl EnumInfo {
    pub fn by_name(name: &str) -> Option<&'static EnumInfo> {
        crate::models::ENUMS.iter().find(|e| e.name == name)
    }
}

/// Implemented by every generated model struct.
pub trait Model: Serialize + DeserializeOwned + for<'r> sqlx::FromRow<'r, PgRow> + Send + Sync + Unpin + 'static {
    /// The model's metadata.
    fn info() -> &'static ModelInfo;

    /// The JSON object `JSON.stringify` wrote for the Prisma row (Redis cache format).
    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Reads a JSON object written by the JS bot or by `to_json`. Unknown keys are ignored, absent optional
    /// fields are `None`; anything else that does not fit is an error (callers treat it as a cache miss).
    fn from_json(value: &serde_json::Value) -> Result<Self, DbError> {
        Self::deserialize_json(value)
    }

    #[doc(hidden)]
    fn deserialize_json(value: &serde_json::Value) -> Result<Self, DbError> {
        serde_json::from_value(value.clone()).map_err(|e| DbError::Json(e.to_string()))
    }

    /// Reads a raw Redis string (the JSON text).
    fn from_json_str(raw: &str) -> Result<Self, DbError> {
        serde_json::from_str(raw).map_err(|e| DbError::Json(e.to_string()))
    }
}

/// The metadata of a model by its Prisma name.
pub fn model_info(name: &str) -> Option<&'static ModelInfo> {
    crate::models::MODELS.iter().copied().find(|m| m.name == name || m.table == name)
}
