//! `fhir-pipes map`: run and check FHIR Mapping Language maps.

use std::path::PathBuf;

use serde_json::Value;

use super::MapPackage;
use crate::error::{PipesError, Result};
use crate::fhir::ndjson::{NdjsonReader, NdjsonWriter};
use crate::fs::check_out;

#[derive(clap::Subcommand, Debug)]
pub enum MapCommand {
    /// Run a map over every record of an NDJSON file and write the resources it produces
    Run(RunArgs),
    /// Report the syntax and type errors of the maps in a folder
    Check(CheckArgs),
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Folder of maps (*.map), StructureDefinitions and ConceptMaps (JSON); repeatable
    #[arg(long = "maps", value_name = "DIR", required = true)]
    pub maps: Vec<PathBuf>,
    /// Name or canonical URL of the map to run; not needed when the folders hold one map
    #[arg(long)]
    pub map: Option<String>,
    /// Input NDJSON, one record per line; a line with no resourceType is given the map's source type
    #[arg(long = "in", value_name = "FILE")]
    pub input: PathBuf,
    /// Output NDJSON of the resources produced: the entries of a Bundle target, or the target itself
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(clap::Args, Debug)]
pub struct CheckArgs {
    /// Folder of maps (*.map), StructureDefinitions and ConceptMaps (JSON); repeatable
    #[arg(long = "maps", value_name = "DIR", required = true)]
    pub maps: Vec<PathBuf>,
}

pub fn run(command: &MapCommand) -> anyhow::Result<()> {
    match command {
        MapCommand::Run(args) => Ok(run_map(args)?),
        MapCommand::Check(args) => check(args),
    }
}

fn run_map(args: &RunArgs) -> Result<()> {
    check_out(&args.out)?;
    let package = MapPackage::load(&args.maps, &[])?;
    let map = match &args.map {
        Some(name) => package.map_named(name)?,
        None => package.only_map()?,
    };
    // The type a record is read as when it does not name one.
    let source_type =
        super::first_source_url(map).and_then(|url| url.rsplit('/').next().map(str::to_string));
    let mut out = NdjsonWriter::create(&args.out)?;
    let mut records = 0usize;
    let mut warnings = 0usize;
    for line in NdjsonReader::open(&args.input)? {
        let line = line?;
        let mut record: Value = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!(
                "{}: line {}: {e}",
                args.input.display(),
                line.number
            ))
        })?;
        if record.get("resourceType").is_none()
            && let (Some(object), Some(source_type)) = (record.as_object_mut(), &source_type)
        {
            object.insert("resourceType".into(), Value::String(source_type.clone()));
        }
        let output = package.transform(map, &record).map_err(|e| {
            PipesError::Environment(format!(
                "{}: line {}: {e}",
                args.input.display(),
                line.number
            ))
        })?;
        for warning in &output.warnings {
            warnings += 1;
            eprintln!("line {}: {warning}", line.number);
        }
        for resource in &output.resources {
            out.write(resource)?;
        }
        records += 1;
    }
    let written = out.finish()?;
    println!(
        "Wrote {written} resources from {records} records to {}",
        args.out.display()
    );
    if warnings > 0 {
        println!("{warnings} warnings");
    }
    Ok(())
}

fn check(args: &CheckArgs) -> anyhow::Result<()> {
    let package = MapPackage::load(&args.maps, &[])?;
    let (errors, warnings) = package.check();
    for line in &warnings {
        println!("warning: {line}");
    }
    for line in &errors {
        println!("error: {line}");
    }
    println!(
        "{} maps: {} errors, {} warnings",
        package.map_count(),
        errors.len(),
        warnings.len()
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(crate::error::Findings.into())
    }
}
