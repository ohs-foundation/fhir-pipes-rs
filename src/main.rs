mod cells;
mod cli;
mod dhis2;
mod diff;
mod error;
mod export;
mod extract;
mod fhir;
mod geometry;
mod index;
mod inspect;
mod load;
mod population;
mod profile;
mod raster;
mod report;
mod run;
mod snapshot;
mod transform;
mod views;
mod warehouse;
mod write;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let profile = cli.profile.as_deref();
    // refresh reports anyhow errors with their context chain.
    if let cli::Command::Refresh(args) = &cli.command {
        if let Err(err) = warehouse::run(args, profile) {
            eprintln!("fhir-pipes: {err:#}");
            std::process::exit(1);
        }
        return;
    }
    if let cli::Command::Views(args) = &cli.command {
        if let Err(err) = views::run(args) {
            // validate, lint and test have already printed their findings.
            if !err.is::<views::Failed>() {
                eprintln!("fhir-pipes: {err:#}");
            }
            std::process::exit(1);
        }
        return;
    }
    let result: error::Result<()> = match cli.command {
        cli::Command::Transform(cli::TransformCommand::Locations(args)) => {
            transform::run_transform(&args, profile)
        }
        cli::Command::Inspect(args) => inspect::run_inspect(&args),
        cli::Command::Extract(args) => extract::run_extract(&args),
        cli::Command::Export(args) => export::run_export(&args),
        cli::Command::Run(args) => run::run(&args, profile),
        cli::Command::Diff(args) => diff::run_diff(&args, profile),
        cli::Command::Load(args) => load::run_load(&args),
        cli::Command::Index(args) => cells::run_index(&args, profile),
        cli::Command::Population(args) => population::run_population(&args, profile),
        cli::Command::Dhis2(cmd) => dhis2::run(&cmd, profile),
        cli::Command::Refresh(_) | cli::Command::Views(_) => unreachable!("handled above"),
    };
    if let Err(err) = result {
        eprintln!("fhir-pipes: {err}");
        std::process::exit(err.exit_code());
    }
}
