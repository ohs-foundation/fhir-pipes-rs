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
mod raster;
mod report;
mod run;
mod snapshot;
mod transform;
mod write;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let result: error::Result<()> = match cli.command {
        cli::Command::Transform(args) => transform::run_transform(&args),
        cli::Command::Inspect(args) => inspect::run_inspect(&args),
        cli::Command::Extract(args) => extract::run_extract(&args),
        cli::Command::Export(args) => export::run_export(&args),
        cli::Command::Run(args) => run::run(&args),
        cli::Command::Diff(args) => diff::run_diff(&args),
        cli::Command::Load(args) => load::run_load(&args),
        cli::Command::Index(args) => cells::run_index(&args),
        cli::Command::Population(args) => population::run_population(&args),
        cli::Command::Dhis2(cmd) => dhis2::run(&cmd),
    };
    if let Err(err) = result {
        eprintln!("fhir-pipes: {err}");
        std::process::exit(err.exit_code());
    }
}
