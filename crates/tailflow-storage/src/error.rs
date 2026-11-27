use rusqlite::ErrorCode;
use std::fmt;
use std::path::PathBuf;

/// Everything that can go wrong opening or using the store.
///
/// `Display` is safe to show a local user (it may contain the database path,
/// never row contents). HTTP handlers must use [`StorageError::public_message`]
/// instead, which omits SQL and paths.
#[derive(Debug)]
#[non_exhaustive]
pub enum StorageError {
    /// The configured path cannot be created or opened.
    CannotOpen {
        path: PathBuf,
        detail: String,
    },
    /// The file exists but is not a SQLite database (or is encrypted).
    NotADatabase {
        path: PathBuf,
    },
    /// A SQLite database that TailFlow did not create. Never written to.
    ForeignDatabase {
        path: PathBuf,
        application_id: i32,
    },
    /// Created by a newer TailFlow. Never downgraded or reset.
    NewerSchema {
        found: i32,
        supported: i32,
    },
    /// Integrity check failed. The file is left untouched.
    Corrupt {
        path: PathBuf,
        detail: String,
    },
    ReadOnly {
        path: PathBuf,
    },
    DiskFull,
    /// The writer thread is gone (shut down or crashed).
    WriterStopped,
    /// A query exceeded its time budget.
    QueryTimeout,
    /// The request is invalid; message is safe to show any caller.
    InvalidQuery(String),
    FullTextDisabled,
    Config(String),
    Sql(rusqlite::Error),
    Io(std::io::Error),
}

impl StorageError {
    /// Short, stable machine-readable kind.
    pub fn kind(&self) -> &'static str {
        match self {
            StorageError::CannotOpen { .. } => "cannot_open",
            StorageError::NotADatabase { .. } => "not_a_database",
            StorageError::ForeignDatabase { .. } => "foreign_database",
            StorageError::NewerSchema { .. } => "newer_schema",
            StorageError::Corrupt { .. } => "corrupt",
            StorageError::ReadOnly { .. } => "read_only",
            StorageError::DiskFull => "disk_full",
            StorageError::WriterStopped => "writer_stopped",
            StorageError::QueryTimeout => "query_timeout",
            StorageError::InvalidQuery(_) => "invalid_query",
            StorageError::FullTextDisabled => "full_text_disabled",
            StorageError::Config(_) => "config",
            StorageError::Sql(_) => "sql",
            StorageError::Io(_) => "io",
        }
    }

    /// Message for API responses: no SQL text, no file paths, no row data.
    pub fn public_message(&self) -> String {
        match self {
            StorageError::InvalidQuery(m) => m.clone(),
            StorageError::QueryTimeout => {
                "query exceeded its time budget; narrow the filter or time range".into()
            }
            StorageError::FullTextDisabled => {
                "full-text search is disabled ([storage] full_text_search = false)".into()
            }
            StorageError::NewerSchema { found, supported } => {
                format!("database schema {found} is newer than this build supports ({supported})")
            }
            StorageError::DiskFull => "storage is full".into(),
            StorageError::WriterStopped => "storage writer is not running".into(),
            other => format!("storage error ({})", other.kind()),
        }
    }

    pub(crate) fn from_open(path: &std::path::Path, e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(ErrorCode::NotADatabase) => StorageError::NotADatabase { path: path.into() },
            Some(ErrorCode::DatabaseCorrupt) => StorageError::Corrupt {
                path: path.into(),
                detail: e.to_string(),
            },
            Some(ErrorCode::ReadOnly) => StorageError::ReadOnly { path: path.into() },
            Some(ErrorCode::DiskFull) => StorageError::DiskFull,
            _ => StorageError::CannotOpen {
                path: path.into(),
                detail: e.to_string(),
            },
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::CannotOpen { path, detail } => {
                write!(f, "cannot open {}: {detail}", path.display())
            }
            StorageError::NotADatabase { path } => write!(
                f,
                "{} is not a SQLite database; it was left untouched. Move it aside or set [storage] path",
                path.display()
            ),
            StorageError::ForeignDatabase {
                path,
                application_id,
            } => write!(
                f,
                "{} is a SQLite database not created by TailFlow (application_id {application_id:#x}); refusing to write to it",
                path.display()
            ),
            StorageError::NewerSchema { found, supported } => write!(
                f,
                "database schema version {found} is newer than this TailFlow supports ({supported}); upgrade TailFlow or point [storage] path elsewhere"
            ),
            StorageError::Corrupt { path, detail } => write!(
                f,
                "integrity check failed for {}: {detail}. The file was left untouched; run `tailflow-daemon storage check`",
                path.display()
            ),
            StorageError::ReadOnly { path } => {
                write!(f, "{} is read-only", path.display())
            }
            StorageError::DiskFull => write!(f, "disk or database is full"),
            StorageError::WriterStopped => write!(f, "storage writer is not running"),
            StorageError::QueryTimeout => write!(f, "query exceeded its time budget"),
            StorageError::InvalidQuery(m) => write!(f, "invalid query: {m}"),
            StorageError::FullTextDisabled => write!(f, "full-text search is disabled"),
            StorageError::Config(m) => write!(f, "invalid storage configuration: {m}"),
            StorageError::Sql(e) => write!(f, "sqlite: {e}"),
            StorageError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Sql(e) => Some(e),
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(ErrorCode::DiskFull) => StorageError::DiskFull,
            Some(ErrorCode::OperationInterrupted) => StorageError::QueryTimeout,
            _ => StorageError::Sql(e),
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

pub type Result<T, E = StorageError> = std::result::Result<T, E>;
