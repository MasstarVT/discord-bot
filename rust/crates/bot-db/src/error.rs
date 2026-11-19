//! The error type of the database layer.

/// Everything bot-db can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The database (or the driver) reported an error.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A column name that the model does not have.
    #[error("unknown column {column:?} on model {model}")]
    UnknownColumn { model: String, column: String },
    /// A required column without a default was not given.
    #[error("missing required column {column:?} on model {model}")]
    MissingColumn { model: String, column: String },
    /// A value that does not fit the column's type.
    #[error("invalid value for {column}: {reason}")]
    InvalidValue { column: String, reason: String },
    /// The same column was given twice.
    #[error("column {0:?} given more than once")]
    DuplicateColumn(String),
    /// An UPDATE or locking SELECT without a WHERE clause.
    #[error("refusing to run {0} without a WHERE clause")]
    EmptyWhere(&'static str),
    /// `DATABASE_URL` could not be used.
    #[error("invalid DATABASE_URL: {0}")]
    Config(String),
    /// JSON that does not match the model.
    #[error("invalid JSON: {0}")]
    Json(String),
    /// The migration runner refused to continue.
    #[error("migration error: {0}")]
    Migration(String),
    /// A timeout (transaction `maxWait` / `timeout`, advisory lock wait, connect).
    #[error("timed out: {0}")]
    Timeout(String),
}

impl DbError {
    /// The PostgreSQL SQLSTATE of a database error (`23505` for a unique violation, ...).
    pub fn sqlstate(&self) -> Option<String> {
        match self {
            DbError::Sqlx(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()),
            _ => None,
        }
    }

    /// `true` for a unique-constraint violation (Prisma's P2002).
    pub fn is_unique_violation(&self) -> bool {
        self.sqlstate().as_deref() == Some("23505")
    }

    /// `true` when a row the statement needed was not found (Prisma's P2025 for update/delete by id).
    pub fn is_not_found(&self) -> bool {
        matches!(self, DbError::Sqlx(sqlx::Error::RowNotFound))
    }
}
