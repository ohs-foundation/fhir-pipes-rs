//! One error type for the whole binary. `Usage` is anything the operator can
//! fix (bad path, malformed input); everything else is an environment fault.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PipesError {
    #[error("{0}")]
    Usage(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("{path}: {source}")]
    ParquetAt {
        path: PathBuf,
        #[source]
        source: parquet::errors::ParquetError,
    },
    #[error("{0}")]
    Environment(String),
    /// A check ran and found problems it has already printed.
    #[error("{0}")]
    Findings(String),
}

impl PipesError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        PipesError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn parquet_at(path: impl Into<PathBuf>, source: parquet::errors::ParquetError) -> Self {
        PipesError::ParquetAt {
            path: path.into(),
            source,
        }
    }

    /// Exit status: 2 for operator-fixable problems, 1 for everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            PipesError::Usage(_) => 2,
            _ => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, PipesError>;
