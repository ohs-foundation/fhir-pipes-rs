mod calendar;
mod cli;
mod dhis2;
mod diff;
mod error;
mod export;
mod extract;
mod fhir;
mod fs;
mod geometry;
mod http;
mod import;
mod index;
mod inspect;
mod load;
mod population;
mod profile;
mod raster;
mod redact;
mod report;
mod run;
#[cfg(feature = "postgres")]
mod runtime;
#[cfg(unix)]
mod serve;
mod snapshot;
mod stop;
mod sync;
mod terminology;
mod transform;
mod views;
mod warehouse;
mod work;
mod write;

use clap::Parser;

// mimalloc in place of the system allocator: much less time in malloc and free
// for view evaluation and the Location transform. musl's malloc takes one lock
// for all threads, which made the views many times slower in the Linux builds.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(err) = run(cli) {
        // A check that failed has already printed its findings.
        if !err.is::<error::Findings>() {
            eprintln!("fhir-pipes: {}", error::message(&err));
        }
        std::process::exit(error::exit_code(&err));
    }
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
        cli::Command::Dhis2(cmd) => dhis2::run(&cmd, profile),
        cli::Command::Profile(cli::ProfileCommand::Check(args)) => {
            profile::check::run(&args, profile)
        }
        cli::Command::Import(cmd) => import::run(&cmd, profile),
        cli::Command::Refresh(args) => warehouse::run(&args, profile),
        #[cfg(unix)]
        cli::Command::Serve(args) => serve::run(&args),
        cli::Command::Views(args) => views::run(&args),
        cli::Command::Terminology(command) => terminology::run(&command),
    }
}
