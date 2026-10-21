//! Errors. Every command returns an `anyhow::Error`; much of the code below
//! the commands returns a `PipesError`, which `?` carries into one. An error
//! the operator can fix (a bad flag, path or input) is a `PipesError::Usage`
//! or is marked with `usage`, and exits 2; everything else exits 1. A check
//! that has already printed its findings returns `Findings`, which exits 1
//! and prints nothing more.

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

/// An error that the operator can fix, in code that uses anyhow: a bad flag,
/// an invalid or unreadable config file or ViewDefinition. It exits 2, as
/// `PipesError::Usage` does; its message is the wrapped error's, context and
/// all.
#[derive(Debug)]
pub struct UsageError(anyhow::Error);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&message(&self.0))
    }
}

impl std::error::Error for UsageError {}

/// `e` marked as the operator's to fix.
pub fn usage(e: impl Into<anyhow::Error>) -> anyhow::Error {
    anyhow::Error::new(UsageError(e.into()))
}

/// A check ran and found problems it has already printed: the command
/// fails, exiting 1, with no message of its own.
#[derive(Debug)]
pub struct Findings;

impl std::fmt::Display for Findings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed")
    }
}

impl std::error::Error for Findings {}

/// The message for `err`: its context chain joined with `: `, as `{:#}`
/// writes it, except that the chain stops at a `PipesError`. Its message
/// already holds its cause (`path: cause`), which `{:#}` would repeat.
pub fn message(err: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for e in err.chain() {
        parts.push(e.to_string());
        if e.is::<PipesError>() {
            break;
        }
    }
    parts.join(": ")
}

/// Exit status of an error, by the rule `PipesError::exit_code` follows: 2
/// when a `UsageError` or a `PipesError::Usage` is in its chain, 1 for
/// everything else.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    let is_usage = err.chain().any(|e| {
        e.is::<UsageError>()
            || e.downcast_ref::<PipesError>()
                .is_some_and(|p| p.exit_code() == 2)
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
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn a_pipes_error_is_not_followed_by_its_cause() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = anyhow::Error::from(PipesError::io("/x", io));
        assert_eq!(message(&err), "/x: gone");
        assert_eq!(message(&err.context("loading")), "loading: /x: gone");
        let err = anyhow::anyhow!("gone")
            .context("reading /x")
            .context("loading");
        assert_eq!(message(&err), "loading: reading /x: gone");
        let err = usage(anyhow::Error::from(PipesError::io(
            "/x",
            std::io::Error::other("gone"),
        )));
        assert_eq!(message(&err.context("views")), "views: /x: gone");
    }

    #[test]
    fn a_pipes_usage_error_in_the_chain_exits_2_and_others_1() {
        let err = anyhow::Error::from(PipesError::Usage("bad".into())).context("locations");
        assert_eq!(exit_code(&err), 2);
        let err = anyhow::Error::from(PipesError::Environment("down".into()));
        assert_eq!(exit_code(&err), 1);
        assert_eq!(exit_code(&anyhow::anyhow!("duckdb failed")), 1);
    }
}
