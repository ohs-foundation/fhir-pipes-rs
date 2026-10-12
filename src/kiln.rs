//! The FHIR side, through the kiln command: the location registry and the
//! NDJSON downloads the views read.

use std::process::Command;

use anyhow::Result;

use crate::config::Settings;
use crate::tools;

/// A kiln command for the configured server. kiln gets and renews its own
/// tokens from the KILN_* client credentials.
fn kiln(s: &Settings, subcommand: &str) -> Command {
    let mut cmd = Command::new(&s.kiln);
    cmd.arg(subcommand).args(["--server", &s.server]);
    if let Some(auth) = &s.auth {
        cmd.env("KILN_CLIENT_ID", &auth.client_id)
            .env("KILN_CLIENT_SECRET", &auth.client_secret)
            .env("KILN_TOKEN_URL", &auth.token_url)
            .env_remove("KILN_TOKEN");
    }
    cmd
}

/// `kiln run`: incremental extract into raw/<server>/locations, transform into
/// parquet/locations. `full` re-extracts every Location (the incremental
/// extract cannot see deletions).
pub fn locations(s: &Settings) -> Result<()> {
    println!(
        "== locations: kiln run ({} → {} → {})",
        s.server,
        s.raw_dir().join("locations").display(),
        s.parquet_dir().join("locations").display()
    );
    let mut cmd = kiln(s, "run");
    cmd.arg("--snapshot")
        .arg(s.raw_dir().join("locations"))
        .arg("--out")
        .arg(s.parquet_dir());
    if s.full {
        cmd.arg("--full");
    }
    tools::run(&mut cmd)
}

/// `kiln export`: every resource of each type to raw/<server>/<Type>.ndjson,
/// in one call.
pub fn export(s: &Settings, types: &[&str]) -> Result<()> {
    if types.is_empty() {
        return Ok(());
    }
    println!("   export {} → {}", types.join(", "), s.raw_dir().display());
    let mut cmd = kiln(s, "export");
    for t in types {
        cmd.args(["--type", t]);
    }
    cmd.arg("--out").arg(s.raw_dir());
    tools::run(&mut cmd)
}
