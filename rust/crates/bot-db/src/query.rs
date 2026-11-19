//! Dynamic INSERT / UPDATE / SELECT builders over the generated model metadata.
//!
//! The settings service, `/settings` and the web dashboard write `GuildSettings` by field name; these builders turn
//! `(column, serde_json::Value)` pairs into correctly typed binds, apply the defaults Prisma applied in the client
//! (`cuid()`, `uuid()`, `now()`, `@updatedAt`) and reject unknown columns.
//!
//! ```ignore
//! let q = UpdateBuilder::new(&GUILD_SETTINGS_INFO)
//!     .set("levelingEnabled", json!(true))?
//!     .filter("guildId", json!(guild_id))?
//!     .build()?;
//! let row: GuildSettings = q.fetch_one(&pool).await?;
//! ```

use chrono::NaiveDateTime;
use serde_json::Value;
use sqlx::postgres::{PgArguments, PgQueryResult, PgRow};
use sqlx::{Arguments, AssertSqlSafe, PgExecutor, Postgres};

use crate::schema::{ClientDefault, ColumnInfo, EnumInfo, ModelInfo, ScalarKind};
use crate::serde_js::{clamp_i32, from_epoch_ms, integral_f64_to_i32, parse_js_date};
use crate::{DbError, quote_ident};

/// A typed bind parameter. `None` binds a NULL of the right type.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Text(Option<String>),
    Int(Option<i32>),
    BigInt(Option<i64>),
    Float(Option<f64>),
    Bool(Option<bool>),
    /// `timestamp(3)` without zone, holding UTC.
    Timestamp(Option<NaiveDateTime>),
    /// `jsonb`. `None` is SQL NULL (Prisma's `DbNull`), `Some(Value::Null)` is the JSON literal `null`.
    Json(Option<Value>),
    TextArray(Vec<String>),
    IntArray(Vec<i32>),
    BigIntArray(Vec<i64>),
    FloatArray(Vec<f64>),
    BoolArray(Vec<bool>),
    TimestampArray(Vec<NaiveDateTime>),
    /// A PostgreSQL enum label, bound as text and cast with `::"Type"`.
    Enum { type_name: &'static str, value: Option<String> },
}

impl SqlValue {
    /// `true` for a NULL bind.
    pub fn is_null(&self) -> bool {
        matches!(
            self,
            SqlValue::Text(None)
                | SqlValue::Int(None)
                | SqlValue::BigInt(None)
                | SqlValue::Float(None)
                | SqlValue::Bool(None)
                | SqlValue::Timestamp(None)
                | SqlValue::Json(None)
                | SqlValue::Enum { value: None, .. }
        )
    }

    /// The placeholder text for parameter `n` (1-based), with the cast the value needs.
    pub fn placeholder(&self, n: usize) -> String {
        match self {
            SqlValue::Enum { type_name, .. } => format!("${n}::{}", quote_ident(type_name)),
            _ => format!("${n}"),
        }
    }

    /// Adds the value to an argument list.
    pub fn add_to(self, args: &mut PgArguments) -> Result<(), DbError> {
        let r = match self {
            SqlValue::Text(v) => args.add(v),
            SqlValue::Int(v) => args.add(v),
            SqlValue::BigInt(v) => args.add(v),
            SqlValue::Float(v) => args.add(v),
            SqlValue::Bool(v) => args.add(v),
            SqlValue::Timestamp(v) => args.add(v),
            SqlValue::Json(v) => args.add(v),
            SqlValue::TextArray(v) => args.add(v),
            SqlValue::IntArray(v) => args.add(v),
            SqlValue::BigIntArray(v) => args.add(v),
            SqlValue::FloatArray(v) => args.add(v),
            SqlValue::BoolArray(v) => args.add(v),
            SqlValue::TimestampArray(v) => args.add(v),
            SqlValue::Enum { value, .. } => args.add(value),
        };
        r.map_err(|e| DbError::InvalidValue { column: "bind".into(), reason: e.to_string() })
    }

    /// A UTC timestamp bind.
    pub fn timestamp(dt: chrono::DateTime<chrono::Utc>) -> Self {
        SqlValue::Timestamp(Some(crate::serde_js::truncate_ms(dt).naive_utc()))
    }
}

fn invalid(col: &ColumnInfo, reason: impl Into<String>) -> DbError {
    DbError::InvalidValue { column: col.name.to_string(), reason: reason.into() }
}

fn type_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn scalar_int(col: &ColumnInfo, v: &Value) -> Result<i32, DbError> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(clamp_i32(i))
            } else if let Some(u) = n.as_u64() {
                Ok(clamp_i32(i64::try_from(u).unwrap_or(i64::MAX)))
            } else {
                n.as_f64().and_then(integral_f64_to_i32).ok_or_else(|| invalid(col, format!("{n} is not an integer")))
            }
        }
        other => Err(invalid(col, format!("expected an integer, got {}", type_label(other)))),
    }
}

fn scalar_bigint(col: &ColumnInfo, v: &Value) -> Result<i64, DbError> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.2e18).map(|f| f as i64))
            .ok_or_else(|| invalid(col, format!("{n} is not a 64-bit integer"))),
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| invalid(col, format!("{s:?} is not an integer"))),
        other => Err(invalid(col, format!("expected an integer, got {}", type_label(other)))),
    }
}

fn scalar_float(col: &ColumnInfo, v: &Value) -> Result<f64, DbError> {
    v.as_f64().ok_or_else(|| invalid(col, format!("expected a number, got {}", type_label(v))))
}

fn scalar_bool(col: &ColumnInfo, v: &Value) -> Result<bool, DbError> {
    v.as_bool().ok_or_else(|| invalid(col, format!("expected a boolean, got {}", type_label(v))))
}

fn scalar_string(col: &ColumnInfo, v: &Value) -> Result<String, DbError> {
    v.as_str().map(str::to_owned).ok_or_else(|| invalid(col, format!("expected a string, got {}", type_label(v))))
}

fn scalar_time(col: &ColumnInfo, v: &Value) -> Result<NaiveDateTime, DbError> {
    let dt = match v {
        Value::String(s) => parse_js_date(s).ok_or_else(|| invalid(col, format!("invalid date {s:?}")))?,
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .and_then(from_epoch_ms)
            .ok_or_else(|| invalid(col, "date out of range"))?,
        other => return Err(invalid(col, format!("expected a date, got {}", type_label(other)))),
    };
    Ok(dt.naive_utc())
}

fn scalar_enum(col: &ColumnInfo, type_name: &'static str, v: &Value) -> Result<String, DbError> {
    let s = scalar_string(col, v)?;
    let known = EnumInfo::by_name(type_name).is_some_and(|e| e.values.contains(&s.as_str()));
    if !known {
        return Err(invalid(col, format!("{s:?} is not a value of enum {type_name}")));
    }
    Ok(s)
}

fn list_of<T>(col: &ColumnInfo, v: &Value, f: impl Fn(&ColumnInfo, &Value) -> Result<T, DbError>) -> Result<Vec<T>, DbError> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    Err(invalid(col, "null inside a list"))
                } else {
                    f(col, item)
                }
            })
            .collect(),
        other => Err(invalid(col, format!("expected an array, got {}", type_label(other)))),
    }
}

/// Converts a JSON value (what the JS code passed to Prisma, or what a dashboard form posts) into the typed bind for
/// `col`. Int values are clamped to the i32 range; `null` is accepted only on nullable columns (on a `Json?`
/// column it is SQL NULL, Prisma's `DbNull`).
pub fn to_sql_value(col: &ColumnInfo, v: &Value) -> Result<SqlValue, DbError> {
    if col.list {
        if v.is_null() {
            return Err(invalid(col, "a list column cannot be null"));
        }
        return Ok(match col.kind {
            ScalarKind::String => SqlValue::TextArray(list_of(col, v, scalar_string)?),
            ScalarKind::Int => SqlValue::IntArray(list_of(col, v, scalar_int)?),
            ScalarKind::BigInt => SqlValue::BigIntArray(list_of(col, v, scalar_bigint)?),
            ScalarKind::Float => SqlValue::FloatArray(list_of(col, v, scalar_float)?),
            ScalarKind::Boolean => SqlValue::BoolArray(list_of(col, v, scalar_bool)?),
            ScalarKind::DateTime => SqlValue::TimestampArray(list_of(col, v, scalar_time)?),
            ScalarKind::Json | ScalarKind::Enum(_) => {
                return Err(invalid(col, "list columns of this type are not supported"));
            }
        });
    }
    if v.is_null() {
        if !col.nullable {
            return Err(invalid(col, "the column is required and cannot be null"));
        }
        return Ok(null_of(col));
    }
    Ok(match col.kind {
        ScalarKind::String => SqlValue::Text(Some(scalar_string(col, v)?)),
        ScalarKind::Int => SqlValue::Int(Some(scalar_int(col, v)?)),
        ScalarKind::BigInt => SqlValue::BigInt(Some(scalar_bigint(col, v)?)),
        ScalarKind::Float => SqlValue::Float(Some(scalar_float(col, v)?)),
        ScalarKind::Boolean => SqlValue::Bool(Some(scalar_bool(col, v)?)),
        ScalarKind::DateTime => SqlValue::Timestamp(Some(scalar_time(col, v)?)),
        ScalarKind::Json => SqlValue::Json(Some(v.clone())),
        ScalarKind::Enum(t) => SqlValue::Enum { type_name: t, value: Some(scalar_enum(col, t, v)?) },
    })
}

/// A typed NULL for a (non-list) column.
pub fn null_of(col: &ColumnInfo) -> SqlValue {
    match col.kind {
        ScalarKind::String => SqlValue::Text(None),
        ScalarKind::Int => SqlValue::Int(None),
        ScalarKind::BigInt => SqlValue::BigInt(None),
        ScalarKind::Float => SqlValue::Float(None),
        ScalarKind::Boolean => SqlValue::Bool(None),
        ScalarKind::DateTime => SqlValue::Timestamp(None),
        ScalarKind::Json => SqlValue::Json(None),
        ScalarKind::Enum(t) => SqlValue::Enum { type_name: t, value: None },
    }
}

/// The value Prisma's client would have filled in for a column left out of a create (`None` = leave it to the
/// database default, or omit it).
pub fn client_default_value(col: &ColumnInfo) -> Option<SqlValue> {
    match col.client_default {
        ClientDefault::Cuid => Some(SqlValue::Text(Some(crate::new_cuid()))),
        ClientDefault::Uuid => Some(SqlValue::Text(Some(crate::new_uuid()))),
        ClientDefault::Now | ClientDefault::UpdatedAt => Some(SqlValue::timestamp(crate::now_utc())),
        ClientDefault::None | ClientDefault::Database => None,
    }
}

fn resolve(info: &ModelInfo, column: &str) -> Result<&'static ColumnInfo, DbError> {
    info.column(column).ok_or_else(|| DbError::UnknownColumn { model: info.name.to_string(), column: column.to_string() })
}

/// A statement with its typed binds.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltQuery {
    pub sql: String,
    pub args: Vec<SqlValue>,
}

impl BuiltQuery {
    /// The bind list as `PgArguments`.
    pub fn arguments(&self) -> Result<PgArguments, DbError> {
        let mut args = PgArguments::default();
        for v in self.args.iter().cloned() {
            v.add_to(&mut args)?;
        }
        Ok(args)
    }

    /// Runs the statement, returning the number of affected rows.
    pub async fn execute<'e, E: PgExecutor<'e>>(&self, exec: E) -> Result<u64, DbError> {
        let r: PgQueryResult = sqlx::query_with(AssertSqlSafe(self.sql.clone()), self.arguments()?).execute(exec).await?;
        Ok(r.rows_affected())
    }

    /// Runs the statement and decodes exactly one row.
    pub async fn fetch_one<'e, T, E>(&self, exec: E) -> Result<T, DbError>
    where
        T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
        E: PgExecutor<'e>,
    {
        Ok(sqlx::query_as_with::<Postgres, T, _>(AssertSqlSafe(self.sql.clone()), self.arguments()?).fetch_one(exec).await?)
    }

    /// Runs the statement and decodes at most one row.
    pub async fn fetch_optional<'e, T, E>(&self, exec: E) -> Result<Option<T>, DbError>
    where
        T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
        E: PgExecutor<'e>,
    {
        Ok(sqlx::query_as_with::<Postgres, T, _>(AssertSqlSafe(self.sql.clone()), self.arguments()?).fetch_optional(exec).await?)
    }

    /// Runs the statement and decodes every row.
    pub async fn fetch_all<'e, T, E>(&self, exec: E) -> Result<Vec<T>, DbError>
    where
        T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
        E: PgExecutor<'e>,
    {
        Ok(sqlx::query_as_with::<Postgres, T, _>(AssertSqlSafe(self.sql.clone()), self.arguments()?).fetch_all(exec).await?)
    }
}

/// Collects `"a" = $1 AND "b" IS NULL ...` conditions.
#[derive(Debug, Clone, Default)]
struct Filters(Vec<(&'static ColumnInfo, SqlValue)>);

impl Filters {
    fn add(&mut self, info: &ModelInfo, column: &str, value: &Value) -> Result<(), DbError> {
        let col = resolve(info, column)?;
        let v = if value.is_null() {
            null_of(col)
        } else if col.list {
            to_sql_value(col, value)?
        } else {
            // A filter value is compared, not stored: nullability does not apply.
            let mut probe = *col;
            probe.nullable = true;
            to_sql_value(&probe, value)?
        };
        self.0.push((col, v));
        Ok(())
    }

    fn render(&self, sql: &mut String, args: &mut Vec<SqlValue>) {
        if self.0.is_empty() {
            return;
        }
        sql.push_str(" WHERE ");
        let mut first = true;
        for (col, v) in &self.0 {
            if !first {
                sql.push_str(" AND ");
            }
            first = false;
            if v.is_null() {
                sql.push_str(&format!("{} IS NULL", quote_ident(col.name)));
            } else {
                args.push(v.clone());
                sql.push_str(&format!("{} = {}", quote_ident(col.name), v.placeholder(args.len())));
            }
        }
    }
}

/// What to do when an INSERT hits a unique constraint.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum OnConflict {
    /// Fail (the default, like `prisma.x.create`).
    #[default]
    Error,
    /// `ON CONFLICT (cols) DO NOTHING` (no row is returned for a conflict).
    DoNothing(Vec<&'static str>),
    /// `ON CONFLICT (cols) DO UPDATE SET` the given columns (and `@updatedAt`), like `prisma.x.upsert`.
    DoUpdate { target: Vec<&'static str>, set: Vec<(&'static str, SqlValue)> },
}

/// `INSERT INTO "Model" (...) VALUES (...) RETURNING <every column>`, with Prisma's client-side defaults.
#[derive(Debug, Clone)]
pub struct InsertBuilder {
    info: &'static ModelInfo,
    values: Vec<(&'static ColumnInfo, SqlValue)>,
    on_conflict: OnConflict,
    returning: bool,
}

impl InsertBuilder {
    pub fn new(info: &'static ModelInfo) -> Self {
        Self { info, values: Vec::new(), on_conflict: OnConflict::Error, returning: true }
    }

    /// Sets a column (by column or Prisma field name) from a JSON value.
    pub fn value(mut self, column: &str, value: Value) -> Result<Self, DbError> {
        let col = resolve(self.info, column)?;
        if self.values.iter().any(|(c, _)| c.name == col.name) {
            return Err(DbError::DuplicateColumn(col.name.to_string()));
        }
        let v = to_sql_value(col, &value)?;
        self.values.push((col, v));
        Ok(self)
    }

    /// Sets several columns.
    pub fn values<'a>(mut self, pairs: impl IntoIterator<Item = (&'a str, Value)>) -> Result<Self, DbError> {
        for (k, v) in pairs {
            self = self.value(k, v)?;
        }
        Ok(self)
    }

    /// Sets a column from an already typed bind.
    pub fn typed(mut self, column: &str, value: SqlValue) -> Result<Self, DbError> {
        let col = resolve(self.info, column)?;
        if self.values.iter().any(|(c, _)| c.name == col.name) {
            return Err(DbError::DuplicateColumn(col.name.to_string()));
        }
        self.values.push((col, value));
        Ok(self)
    }

    /// `ON CONFLICT (target) DO NOTHING`.
    pub fn on_conflict_do_nothing(mut self, target: &[&str]) -> Result<Self, DbError> {
        let cols = target.iter().map(|c| resolve(self.info, c).map(|c| c.name)).collect::<Result<Vec<_>, _>>()?;
        self.on_conflict = OnConflict::DoNothing(cols);
        Ok(self)
    }

    /// `ON CONFLICT (target) DO UPDATE SET ...` (an upsert). `@updatedAt` is set too, when anything is set: Prisma
    /// sends no UPDATE for `upsert({ update: {} })`, so an empty set leaves the existing row (its `updatedAt`
    /// included) as it is and returns it.
    pub fn on_conflict_update<'a>(mut self, target: &[&str], set: impl IntoIterator<Item = (&'a str, Value)>) -> Result<Self, DbError> {
        let cols = target.iter().map(|c| resolve(self.info, c).map(|c| c.name)).collect::<Result<Vec<_>, _>>()?;
        let mut sets = Vec::new();
        for (k, v) in set {
            let col = resolve(self.info, k)?;
            if sets.iter().any(|(n, _): &(&str, SqlValue)| *n == col.name) {
                return Err(DbError::DuplicateColumn(col.name.to_string()));
            }
            sets.push((col.name, to_sql_value(col, &v)?));
        }
        if !sets.is_empty()
            && let Some(u) = self.info.updated_at_column()
            && !sets.iter().any(|(n, _)| *n == u.name)
        {
            sets.push((u.name, SqlValue::timestamp(crate::now_utc())));
        }
        self.on_conflict = OnConflict::DoUpdate { target: cols, set: sets };
        Ok(self)
    }

    /// Leaves out the RETURNING clause.
    pub fn without_returning(mut self) -> Self {
        self.returning = false;
        self
    }

    /// Builds the statement. Client defaults are generated here; a required column without any default that was not
    /// given is an error.
    pub fn build(self) -> Result<BuiltQuery, DbError> {
        let mut provided = self.values;
        let mut cols: Vec<(&'static ColumnInfo, SqlValue)> = Vec::new();
        for col in self.info.columns {
            if let Some(pos) = provided.iter().position(|(c, _)| c.name == col.name) {
                cols.push(provided.swap_remove(pos));
            } else if let Some(v) = client_default_value(col) {
                cols.push((col, v));
            } else if !col.nullable && !col.has_default && !col.list {
                return Err(DbError::MissingColumn { model: self.info.name.to_string(), column: col.name.to_string() });
            }
        }
        let mut args = Vec::new();
        let names = cols.iter().map(|(c, _)| quote_ident(c.name)).collect::<Vec<_>>().join(", ");
        let mut placeholders = Vec::new();
        for (_, v) in cols {
            args.push(v);
            placeholders.push(args.last().map(|v| v.placeholder(args.len())).unwrap_or_default());
        }
        let mut sql = if names.is_empty() {
            format!("INSERT INTO {} DEFAULT VALUES", self.info.quoted_table())
        } else {
            format!("INSERT INTO {} ({}) VALUES ({})", self.info.quoted_table(), names, placeholders.join(", "))
        };
        match self.on_conflict {
            OnConflict::Error => {}
            OnConflict::DoNothing(target) => {
                let t = target.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
                sql.push_str(&format!(" ON CONFLICT ({t}) DO NOTHING"));
            }
            OnConflict::DoUpdate { target, set } => {
                let t = target.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
                let mut parts = Vec::new();
                for (name, v) in set {
                    args.push(v);
                    let ph = args.last().map(|v| v.placeholder(args.len())).unwrap_or_default();
                    parts.push(format!("{} = {}", quote_ident(name), ph));
                }
                if parts.is_empty() {
                    // `upsert({ update: {} })`: the existing row unchanged, and returned (DO NOTHING would return no
                    // row). The conflict target equals EXCLUDED's, so this writes the same value back.
                    match target.first() {
                        Some(c) => sql.push_str(&format!(" ON CONFLICT ({t}) DO UPDATE SET {q} = EXCLUDED.{q}", q = quote_ident(c))),
                        None => sql.push_str(&format!(" ON CONFLICT ({t}) DO NOTHING")),
                    }
                } else {
                    sql.push_str(&format!(" ON CONFLICT ({t}) DO UPDATE SET {}", parts.join(", ")));
                }
            }
        }
        if self.returning {
            sql.push_str(&format!(" RETURNING {}", self.info.select_list()));
        }
        Ok(BuiltQuery { sql, args })
    }
}

/// `UPDATE "Model" SET ... WHERE ... RETURNING <every column>`. The `@updatedAt` column is set too (to now, unless the
/// caller sets it explicitly) whenever anything is set. An empty WHERE is refused. With nothing to set, Prisma sends
/// no UPDATE: `update({ data: {} })` only reads the row, so this builds that `SELECT`, and `updateMany({ data: {} })`
/// changes nothing (count 0), so without RETURNING it builds a statement that affects no row.
#[derive(Debug, Clone)]
pub struct UpdateBuilder {
    info: &'static ModelInfo,
    set: Vec<(&'static ColumnInfo, SqlValue)>,
    filters: Filters,
    returning: bool,
}

impl UpdateBuilder {
    pub fn new(info: &'static ModelInfo) -> Self {
        Self { info, set: Vec::new(), filters: Filters::default(), returning: true }
    }

    /// Sets a column (by column or Prisma field name) from a JSON value.
    pub fn set(mut self, column: &str, value: Value) -> Result<Self, DbError> {
        let col = resolve(self.info, column)?;
        if self.set.iter().any(|(c, _)| c.name == col.name) {
            return Err(DbError::DuplicateColumn(col.name.to_string()));
        }
        let v = to_sql_value(col, &value)?;
        self.set.push((col, v));
        Ok(self)
    }

    /// Sets several columns.
    pub fn set_all<'a>(mut self, pairs: impl IntoIterator<Item = (&'a str, Value)>) -> Result<Self, DbError> {
        for (k, v) in pairs {
            self = self.set(k, v)?;
        }
        Ok(self)
    }

    /// Sets a column from an already typed bind.
    pub fn set_typed(mut self, column: &str, value: SqlValue) -> Result<Self, DbError> {
        let col = resolve(self.info, column)?;
        if self.set.iter().any(|(c, _)| c.name == col.name) {
            return Err(DbError::DuplicateColumn(col.name.to_string()));
        }
        self.set.push((col, value));
        Ok(self)
    }

    /// Adds `"column" = value` (or `IS NULL` for `null`) to the WHERE clause.
    pub fn filter(mut self, column: &str, value: Value) -> Result<Self, DbError> {
        self.filters.add(self.info, column, &value)?;
        Ok(self)
    }

    /// Leaves out the RETURNING clause.
    pub fn without_returning(mut self) -> Self {
        self.returning = false;
        self
    }

    pub fn build(self) -> Result<BuiltQuery, DbError> {
        if self.filters.0.is_empty() {
            return Err(DbError::EmptyWhere("UPDATE"));
        }
        let mut set = self.set;
        if set.is_empty() {
            let mut args = Vec::new();
            if self.returning {
                let mut sql = format!("SELECT {} FROM {}", self.info.select_list(), self.info.quoted_table());
                self.filters.render(&mut sql, &mut args);
                return Ok(BuiltQuery { sql, args });
            }
            return Ok(BuiltQuery { sql: "SELECT 1 WHERE FALSE".to_string(), args });
        }
        if let Some(u) = self.info.updated_at_column()
            && !set.iter().any(|(c, _)| c.name == u.name)
        {
            set.push((u, SqlValue::timestamp(crate::now_utc())));
        }
        let mut args = Vec::new();
        let mut parts = Vec::new();
        for (col, v) in set {
            args.push(v);
            let ph = args.last().map(|v| v.placeholder(args.len())).unwrap_or_default();
            parts.push(format!("{} = {}", quote_ident(col.name), ph));
        }
        let mut sql = format!("UPDATE {} SET {}", self.info.quoted_table(), parts.join(", "));
        self.filters.render(&mut sql, &mut args);
        if self.returning {
            sql.push_str(&format!(" RETURNING {}", self.info.select_list()));
        }
        Ok(BuiltQuery { sql, args })
    }
}

/// Row lock clause of a SELECT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockMode {
    #[default]
    None,
    ForUpdate,
    ForShare,
    ForUpdateSkipLocked,
    ForUpdateNoWait,
}

impl LockMode {
    pub fn sql(self) -> &'static str {
        match self {
            LockMode::None => "",
            LockMode::ForUpdate => " FOR UPDATE",
            LockMode::ForShare => " FOR SHARE",
            LockMode::ForUpdateSkipLocked => " FOR UPDATE SKIP LOCKED",
            LockMode::ForUpdateNoWait => " FOR UPDATE NOWAIT",
        }
    }
}

/// `SELECT <every column> FROM "Model" WHERE ... [ORDER BY ...] [LIMIT n] [FOR UPDATE ...]`.
#[derive(Debug, Clone)]
pub struct SelectBuilder {
    info: &'static ModelInfo,
    filters: Filters,
    order_by: Vec<(&'static str, bool)>,
    limit: Option<i64>,
    lock: LockMode,
}

impl SelectBuilder {
    pub fn new(info: &'static ModelInfo) -> Self {
        Self { info, filters: Filters::default(), order_by: Vec::new(), limit: None, lock: LockMode::None }
    }

    /// Adds `"column" = value` (or `IS NULL`).
    pub fn filter(mut self, column: &str, value: Value) -> Result<Self, DbError> {
        self.filters.add(self.info, column, &value)?;
        Ok(self)
    }

    /// `ORDER BY "column" ASC|DESC`.
    pub fn order_by(mut self, column: &str, ascending: bool) -> Result<Self, DbError> {
        let col = resolve(self.info, column)?;
        self.order_by.push((col.name, ascending));
        Ok(self)
    }

    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    /// A row lock; refused without a WHERE clause.
    pub fn lock(mut self, mode: LockMode) -> Self {
        self.lock = mode;
        self
    }

    pub fn build(self) -> Result<BuiltQuery, DbError> {
        if self.lock != LockMode::None && self.filters.0.is_empty() {
            return Err(DbError::EmptyWhere("a locking SELECT"));
        }
        let mut args = Vec::new();
        let mut sql = format!("SELECT {} FROM {}", self.info.select_list(), self.info.quoted_table());
        self.filters.render(&mut sql, &mut args);
        if !self.order_by.is_empty() {
            let o = self
                .order_by
                .iter()
                .map(|(c, asc)| format!("{} {}", quote_ident(c), if *asc { "ASC" } else { "DESC" }))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" ORDER BY {o}"));
        }
        if let Some(n) = self.limit {
            sql.push_str(&format!(" LIMIT {n}"));
        }
        sql.push_str(self.lock.sql());
        Ok(BuiltQuery { sql, args })
    }
}

/// `DELETE FROM "Model" WHERE ...` (an empty WHERE is refused).
pub fn delete_where<'a>(info: &'static ModelInfo, filters: impl IntoIterator<Item = (&'a str, Value)>) -> Result<BuiltQuery, DbError> {
    let mut f = Filters::default();
    for (k, v) in filters {
        f.add(info, k, &v)?;
    }
    if f.0.is_empty() {
        return Err(DbError::EmptyWhere("DELETE"));
    }
    let mut args = Vec::new();
    let mut sql = format!("DELETE FROM {}", info.quoted_table());
    f.render(&mut sql, &mut args);
    Ok(BuiltQuery { sql, args })
}
