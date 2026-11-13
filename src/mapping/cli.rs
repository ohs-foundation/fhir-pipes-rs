//! `fhir-pipes map`: run and check FHIR Mapping Language maps.

use std::path::PathBuf;

use serde_json::Value;

use super::{BATCH, MapPackage};
use crate::error::{PipesError, Result};
use crate::fhir::ndjson::{Line, NdjsonReader, NdjsonWriter};
use crate::fs::check_out;

/// The logical models of the DHIS2 records that fhir-pipes-dhis2 writes
/// (examples/import/icr-dhis2/models), built in so that `map run` and `map
/// check` read a folder of DHIS2 maps without its models. `import` does not
/// use them: it reads the models in the package.
pub(crate) const BUILT_IN: [&str; 3] = [
    include_str!("../../examples/import/icr-dhis2/models/StructureDefinition-DHIS2OrgUnit.json"),
    include_str!("../../examples/import/icr-dhis2/models/StructureDefinition-DHIS2Indicator.json"),
    include_str!("../../examples/import/icr-dhis2/models/StructureDefinition-DHIS2Value.json"),
];

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
    /// Threads that run the map (default: the number of CPUs). The output keeps the input order whatever the count
    #[arg(long)]
    pub threads: Option<usize>,
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
    let mut package = MapPackage::load(&args.maps, &BUILT_IN)?;
    let (values, manifests) = crate::import::package::map_parameter_defaults(&args.maps)?;
    package.set_parameters(&values);
    for m in &manifests {
        eprintln!("parameters: the defaults in {}", m.display());
    }
    let map = match &args.map {
        Some(name) => package.map_named(name)?,
        None => package.only_map()?,
    };
    // The type a record is read as when it does not name one.
    let source_type =
        super::first_source_url(map).and_then(|url| url.rsplit('/').next().map(str::to_string));
    let threads = args.threads.unwrap_or_else(crate::work::default_threads);
    let read = |line: Result<Line>| -> Result<(usize, Value)> {
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
        Ok((line.number, record))
    };
    let mut out = NdjsonWriter::create(&args.out)?;
    let mut lines = NdjsonReader::open(&args.input)?;
    let mut records = 0usize;
    let mut warnings = 0usize;
    loop {
        // A batch of records, transformed on several threads and written in
        // input order. A line that cannot be read ends the batch, and its
        // error is returned once the lines before it are written.
        let mut batch = Vec::with_capacity(BATCH);
        let mut unread = None;
        for line in lines.by_ref() {
            match read(line) {
                Ok(numbered) => batch.push(numbered),
                Err(e) => {
                    unread = Some(e);
                    break;
                }
            }
            if batch.len() == BATCH {
                break;
            }
        }
        let sources: Vec<&Value> = batch.iter().map(|(_, record)| record).collect();
        let outputs = package.transform_all(map, &sources, threads);
        for ((number, _), output) in batch.iter().zip(outputs) {
            let output = output.map_err(|e| {
                PipesError::Environment(format!("{}: line {number}: {e}", args.input.display()))
            })?;
            for warning in &output.warnings {
                warnings += 1;
                eprintln!("line {number}: {warning}");
            }
            for resource in &output.resources {
                out.write(resource)?;
            }
            records += 1;
        }
        if let Some(e) = unread {
            return Err(e);
        }
        if batch.len() < BATCH {
            break;
        }
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
    let package = MapPackage::load(&args.maps, &BUILT_IN)?;
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
