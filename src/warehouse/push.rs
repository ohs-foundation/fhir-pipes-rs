//! Mirroring the warehouse to object storage with rclone, so dashboards and
//! other consumers read the same files from the cloud. raw/ is never pushed.

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::catalog;
use super::config::{Hook, Settings};
use super::tools;

pub fn push(s: &Settings) -> Result<()> {
    let p = s
        .push
        .as_ref()
        .context("no [push] section in the config: set remote and bucket")?;
    let buckets = tools::output(Command::new(&s.rclone).args(["lsd", &format!("{}:", p.remote)]))?;
    if !buckets
        .lines()
        .any(|l| l.split_whitespace().last() == Some(p.bucket.as_str()))
    {
        bail!(
            "bucket {} not found on rclone remote {}",
            p.bucket,
            p.remote
        );
    }
    println!(
        "== push {} → {}:{}{}",
        s.data.display(),
        p.remote,
        p.bucket,
        if s.dry_run { " (dry run)" } else { "" }
    );
    for dir in &p.dirs {
        let src = s.data.join(dir);
        if !src.is_dir() {
            continue;
        }
        let mut cmd = Command::new(&s.rclone);
        cmd.arg("sync");
        if s.dry_run {
            cmd.arg("--dry-run");
        }
        cmd.args([
            "--checksum",
            "--fast-list",
            "--transfers",
            "8",
            "--stats-one-line",
        ]);
        for pattern in &p.exclude {
            cmd.args(["--exclude", pattern]);
        }
        cmd.arg(&src)
            .arg(format!("{}:{}/{dir}", p.remote, p.bucket));
        tools::run(&mut cmd)?;
        println!("   {dir}/");
    }
    for file in &p.files {
        let src = s.data.join(file);
        if !src.is_file() {
            continue;
        }
        let mut cmd = Command::new(&s.rclone);
        cmd.arg("copyto");
        if s.dry_run {
            cmd.arg("--dry-run");
        }
        cmd.arg(&src)
            .arg(format!("{}:{}/{file}", p.remote, p.bucket));
        tools::run(&mut cmd)?;
        println!("   {file}");
    }
    // The catalog goes last, once the files its views name are uploaded.
    // catalog.duckdb pointing at the bucket can only be built now: DuckDB reads
    // each table's schema from the files when it creates the view.
    if let Some(c) = &s.catalog {
        let mut files = vec![catalog::SQL_FILE];
        if c.duckdb_file {
            if c.base_url.is_some() && !s.dry_run {
                catalog::build_duckdb(s, c)?;
            }
            files.push(catalog::DUCKDB_FILE);
        }
        for file in files {
            let src = s.data.join(file);
            if p.files.iter().any(|f| f == file) || !(src.is_file() || s.dry_run) {
                continue;
            }
            let mut cmd = Command::new(&s.rclone);
            cmd.arg("copyto");
            if s.dry_run {
                cmd.arg("--dry-run");
            }
            cmd.arg(&src)
                .arg(format!("{}:{}/{file}", p.remote, p.bucket));
            tools::run(&mut cmd)?;
            println!("   {file}");
        }
    }
    Ok(())
}

/// Run a hook with the run's details in its environment: DATA, FHIR_BASE,
/// SERVER_NAME and REFRESHED_AT. It runs in the config file's directory.
pub fn hook(s: &Settings, h: &Hook, program: &str, refreshed_at: &str) -> Result<()> {
    println!("== hook {}", h.name);
    tools::run(
        Command::new(program)
            .args(&h.run[1..])
            .current_dir(&s.base_dir)
            .env("DATA", &s.data)
            .env("FHIR_BASE", &s.server)
            .env("SERVER_NAME", &s.server_name)
            .env("REFRESHED_AT", refreshed_at),
    )
    .with_context(|| format!("hook {}", h.name))
}
