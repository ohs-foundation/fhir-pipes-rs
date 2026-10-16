mod cells;
mod cli;
mod dhis2;
mod diff;
mod error;
mod export;
mod extract;
mod fhir;
mod fs;
mod geometry;
mod import;
mod index;
mod inspect;
mod load;
mod population;
mod profile;
mod raster;
mod report;
mod run;
mod snapshot;
mod sync;
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
    let profile = cli.profile.as_deref();
    // refresh and views report anyhow errors with their context chain, and
    // exit as the other commands do: 2 when the operator can fix the error.
    if let cli::Command::Refresh(args) = &cli.command {
        if let Err(err) = warehouse::run(args, profile) {
            eprintln!("fhir-pipes: {err:#}");
            std::process::exit(error::anyhow_exit_code(&err));
        }
        return;
    }
    if let cli::Command::Views(args) = &cli.command {
        if let Err(err) = views::run(args) {
            // validate, lint and test have already printed their findings.
            if !err.is::<views::Failed>() {
                eprintln!("fhir-pipes: {err:#}");
            }
            std::process::exit(error::anyhow_exit_code(&err));
        }
        return;
    }
    let result: error::Result<()> = match cli.command {
        cli::Command::Transform(cli::TransformCommand::Locations(args)) => {
            transform::run_transform(&args, profile)
        }
        cli::Command::Inspect(args) => inspect::run_inspect(&args),
        cli::Command::Extract(args) => profile::Profile::for_snapshot(profile, &args.snapshot)
            .and_then(|p| extract::run_extract(&args, &p.location.boundary_extensions)),
        cli::Command::Export(args) => export::run_export(&args),
        cli::Command::Run(args) => run::run(&args, profile),
        cli::Command::Diff(args) => diff::run_diff(&args, profile),
        cli::Command::Load(args) => load::run_load(&args),
        cli::Command::Sync(args) => sync::run_sync(&args),
        cli::Command::Index(args) => cells::run_index(&args, profile),
        cli::Command::Population(args) => population::run_population(&args, profile),
        cli::Command::Dhis2(cmd) => dhis2::run(&cmd, profile),
        cli::Command::Profile(cli::ProfileCommand::Check(args)) => {
            profile::check::run(&args, profile)
        }
        cli::Command::Import(cmd) => import::run(&cmd, profile),
        cli::Command::Refresh(_) | cli::Command::Views(_) => unreachable!("handled above"),
    };
    if let Err(err) = result {
        eprintln!("fhir-pipes: {err}");
        std::process::exit(err.exit_code());
    }
}
