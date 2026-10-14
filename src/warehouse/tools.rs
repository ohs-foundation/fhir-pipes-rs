//! Running the external tools: duckdb, tippecanoe, rclone and hooks.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Where a tool is: a path containing `/` as given, else the first match on PATH.
pub fn find(tool: &str) -> Option<PathBuf> {
    if tool.contains('/') {
        let p = PathBuf::from(tool);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(tool))
            .find(|p| p.is_file())
    })
}

/// Fail early, before any step runs, when a tool a selected step needs is missing.
pub fn require(tools: &[(&str, &str)]) -> Result<()> {
    let missing: Vec<String> = tools
        .iter()
        .filter(|(tool, _)| find(tool).is_none())
        .map(|(tool, why)| format!("{tool} (for {why})"))
        .collect();
    if !missing.is_empty() {
        bail!("not found: {}", missing.join(", "));
    }
    Ok(())
}

/// Run a command, letting its output through, and fail if it fails.
pub fn run(cmd: &mut Command) -> Result<()> {
    let shown = show(cmd);
    let status = cmd.status().with_context(|| format!("starting {shown}"))?;
    if !status.success() {
        bail!("{shown} failed ({status})");
    }
    Ok(())
}

/// Run a command and return its standard output; standard error passes through.
pub fn output(cmd: &mut Command) -> Result<String> {
    let shown = show(cmd);
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("starting {shown}"))?;
    if !out.status.success() {
        bail!("{shown} failed ({})", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a duckdb SQL script, discarding its result table.
pub fn duckdb(duckdb: &str, sql: &str) -> Result<()> {
    output(Command::new(duckdb).args(["-c", sql])).map(|_| ())
}

/// A SQL string literal for a path.
pub fn sql_str(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

fn show(cmd: &Command) -> String {
    let program = Path::new(cmd.get_program())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let first = cmd
        .get_args()
        .next()
        .map(|a| format!(" {}", a.to_string_lossy()))
        .unwrap_or_default();
    format!("`{program}{first}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_strings_escape_quotes() {
        assert_eq!(sql_str(Path::new("/a/o'b")), "'/a/o''b'");
    }

    #[test]
    fn find_uses_path_and_explicit_paths() {
        assert!(find("sh").is_some());
        assert!(find("/bin/sh").is_some());
        assert!(find("/no/such/tool").is_none());
        assert!(find("no-such-tool-anywhere").is_none());
    }
}
