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

/// An error in `refresh` or `views`, which use anyhow, that the operator can
/// fix: a bad flag, an invalid or unreadable config file or ViewDefinition.
/// It exits 2, as `PipesError::Usage` does; its message is the wrapped
/// error's, context and all.
#[derive(Debug)]
pub struct UsageError(anyhow::Error);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for UsageError {}

/// `e` marked as the operator's to fix.
pub fn usage(e: impl Into<anyhow::Error>) -> anyhow::Error {
    anyhow::Error::new(UsageError(e.into()))
}

/// Exit status of an anyhow error, by the rule `PipesError::exit_code`
/// follows: 2 when a `UsageError` or a `PipesError::Usage` is in its chain,
/// 1 for everything else.
pub fn anyhow_exit_code(err: &anyhow::Error) -> i32 {
    let is_usage = err.chain().any(|e| {
        e.is::<UsageError>() || matches!(e.downcast_ref::<PipesError>(), Some(PipesError::Usage(_)))
    });
    if is_usage { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usage_error_keeps_its_message_and_exits_2() {
        let inner = anyhow::anyhow!("bad key").context("parsing fhir-pipes.toml");
        let plain = format!("{inner:#}");
        let err = usage(inner).context("loading the config");
        assert_eq!(format!("{err:#}"), format!("loading the config: {plain}"));
        assert_eq!(anyhow_exit_code(&err), 2);
    }

    #[test]
    fn a_pipes_usage_error_in_the_chain_exits_2_and_others_1() {
        let err = anyhow::Error::from(PipesError::Usage("bad".into())).context("locations");
        assert_eq!(anyhow_exit_code(&err), 2);
        let err = anyhow::Error::from(PipesError::Environment("down".into()));
        assert_eq!(anyhow_exit_code(&err), 1);
        assert_eq!(anyhow_exit_code(&anyhow::anyhow!("duckdb failed")), 1);
    }
}
