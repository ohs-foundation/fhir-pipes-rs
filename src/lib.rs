//! The fhir-pipes program as a library. `src/main.rs` calls [`main`].
//!
//! The DHIS2 connector, the separate program `fhir-pipes-dhis2`
//! (crates/dhis2), uses the modules that are public here: the FHIR NDJSON
//! files, the HTTP client, the reports, the maps and import packages. The
//! other modules are private to fhir-pipes.

pub mod calendar;
mod cli;
mod diff;
pub mod error;
mod export;
mod external;
mod extract;
pub mod fhir;
pub mod fs;
pub mod geometry;
pub mod http;
pub mod import;
mod index;
mod inspect;
mod library;
mod load;
pub mod mapping;
mod population;
mod profile;
mod raster;
pub mod redact;
pub mod report;
mod run;
pub mod runtime;
#[cfg(unix)]
mod serve;
pub mod snapshot;
mod stop;
mod sync;
mod terminology;
mod transform;
mod views;
mod warehouse;
pub mod work;
mod write;

/// The defaults of `--retries` and `--timeout`, which the connector's
/// options share.
pub use cli::{DEFAULT_RETRIES, DEFAULT_TIMEOUT_SECS};

use clap::Parser;

/// Runs fhir-pipes with the process's arguments, and exits with its code.
pub fn main() {
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => std::process::exit(external_or_error(err)),
    };
    if let Err(err) = run(cli) {
        // A check that failed has already printed its findings.
        if !err.is::<error::Findings>() {
            eprintln!("fhir-pipes: {}", error::message(&err));
        }
        std::process::exit(error::exit_code(&err));
    }
}

/// When clap does not know the first command, runs the external command
/// `fhir-pipes-NAME` if there is one (see `external`), and returns its exit
/// code. Otherwise prints clap's error, and for an unknown command a line
/// saying where `fhir-pipes-NAME` was looked for, and returns clap's code.
fn external_or_error(err: clap::Error) -> i32 {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let name = match (err.kind(), err.get(ContextKind::InvalidSubcommand)) {
        (ErrorKind::InvalidSubcommand, Some(ContextValue::String(name))) => Some(name.clone()),
        _ => None,
    };
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let call = name.as_deref().and_then(|n| external::call(&argv, n));
    if let Some(call) = &call {
        let exe = std::env::current_exe().ok();
        let exe_dir = exe.as_deref().and_then(std::path::Path::parent);
        let path = std::env::var_os("PATH");
        if let Some(program) = external::find(&call.name, exe_dir, path.as_deref()) {
            return external::run(&program, call);
        }
    }
    let _ = err.print();
    if let Some(call) = call {
        eprintln!(
            "fhir-pipes-{} was not found next to fhir-pipes or on the PATH",
            call.name
        );
    }
    err.exit_code()
}

fn run(cli: cli::Cli) -> anyhow::Result<()> {
    let profile = cli.profile.as_deref();
    match cli.command {
        cli::Command::Transform(cli::TransformCommand::Locations(args)) => {
            transform::run_transform(&args, profile)
        }
        cli::Command::Inspect(args) => inspect::run_inspect(&args),
        cli::Command::Extract(args) => {
            let p = profile::Profile::for_snapshot(profile, &args.snapshot)?;
            extract::run_extract(&args, &p.location.boundary_extensions)
        }
        cli::Command::Export(args) => export::run_export(&args),
        cli::Command::Run(args) => run::run(&args, profile),
        cli::Command::Diff(args) => diff::run_diff(&args, profile),
        cli::Command::Load(args) => load::run_load(&args),
        cli::Command::Sync(args) => sync::run_sync(&args),
        cli::Command::Index(args) => index::command::run_index(&args, profile),
        cli::Command::Population(args) => population::run_population(&args, profile),
        cli::Command::Profile(cli::ProfileCommand::Check(args)) => {
            profile::check::run(&args, profile)
        }
        cli::Command::Import(cmd) => import::run(&cmd, profile),
        cli::Command::Refresh(args) => warehouse::run(&args, profile),
        #[cfg(unix)]
        cli::Command::Serve(args) => serve::run(&args),
        cli::Command::Views(args) => views::run(&args),
        cli::Command::Libraries(args) => library::run(&args),
        cli::Command::Terminology(command) => terminology::run(&command),
        cli::Command::Map(command) => mapping::cli::run(&command),
    }
}
