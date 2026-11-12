//! `fhir-pipes dhis2`: read a DHIS2 instance into records, and with an
//! import package into FHIR NDJSON for `fhir-pipes load`.
//!
//! - `extract locations|measures|reports`: org units, indicators and
//!   indicator values as `DHIS2OrgUnit`, `DHIS2Indicator` and `DHIS2Value`
//!   records, written to a folder for `fhir-pipes import run`.
//! - `locations`, `measures`, `reports`: the same records, each run through
//!   the map of an import package (`--package`) that reads its type, so the
//!   command writes resources: Locations and facility Organizations,
//!   Measures, MeasureReports.
//!
//! Every id is deterministic (`INSTANCE-<uid>…`), so re-running replaces the
//! same resources. Nothing is written to DHIS2.

pub mod api;
pub mod calendar;
pub mod expression;
pub mod locations;
pub mod maps;
pub mod measures;
pub mod raw;
pub mod reports;
pub mod rerun;

use std::path::{Path, PathBuf};

use crate::cli::{Dhis2Command, Dhis2ExtractCommand, Dhis2Package};
use crate::error::{PipesError, Result};
use crate::profile::Profile;
use maps::RecordMap;

/// Instance names go into every id, so they are kept short.
pub const MAX_INSTANCE_LEN: usize = 16;

/// No `dhis2` command reads the profile. The URLs the resources carry are
/// the package's parameters. A profile given with a `[dhis2]` section, which
/// these commands read before, gets a warning that it is not read.
pub fn run(cmd: &Dhis2Command, profile: Option<&str>) -> anyhow::Result<()> {
    if let Dhis2Command::Extract(cmd) = cmd {
        return Ok(extract(cmd)?);
    }
    warn_of_dhis2_section(profile);
    match cmd {
        Dhis2Command::Locations(args) => locations::run_locations(args)?,
        Dhis2Command::Measures(args) => measures::run_measures(args)?,
        Dhis2Command::Reports(args) => reports::run_reports(args)?,
        Dhis2Command::Extract(_) => unreachable!("handled above"),
    }
    Ok(())
}

/// One line on stderr when `profile`, a local file, has a `[dhis2]`
/// section. A profile that cannot be read is left to the commands that
/// read it.
fn warn_of_dhis2_section(profile: Option<&str>) {
    let Some(spec) = profile.filter(|s| !s.is_empty() && !s.starts_with("https://")) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(spec) else {
        return;
    };
    if Profile::parse(&text, spec).is_ok_and(|p| p.dhis2.is_some()) {
        eprintln!(
            "warning: profile {spec}: dhis2 no longer reads the [dhis2] section; the package's \
             parameters give those values (change one with --param NAME=VALUE)"
        );
    }
}

/// The package of `dhis2 locations|measures|reports`, and the map in it
/// that reads `model`. Without `--package` (or `$FHIR_PIPES_DHIS2_MAPS`) the
/// command stops.
pub fn package_map(command: &str, package: &Dhis2Package, model: &str) -> Result<RecordMap> {
    let Some(dir) = &package.dir else {
        return Err(PipesError::Usage(format!(
            "dhis2 {command} needs an import package to write resources with: give --package DIR \
             (such as examples/import/icr-dhis2) or set $FHIR_PIPES_DHIS2_MAPS. To write the \
             records without mapping them, run `fhir-pipes dhis2 extract {command}`"
        )));
    };
    RecordMap::load(dir, &package.params, model)
}

/// `dhis2 extract`: the matching command run with its records written to
/// `--out/<RecordType>.ndjson` instead of resources. Its temporary and
/// `--resume` files sit beside that file, as `<RecordType>.ndjson.tmp` and
/// `.resume`, which `import run` does not read.
fn extract(cmd: &Dhis2ExtractCommand) -> Result<()> {
    let file = |dir: &Path, model: &str| -> Result<PathBuf> {
        if dir.exists() && !dir.is_dir() {
            return Err(PipesError::Usage(format!(
                "--out {}: is not a folder; dhis2 extract writes one file per record type in it",
                dir.display()
            )));
        }
        std::fs::create_dir_all(dir).map_err(|e| PipesError::io(dir, e))?;
        Ok(dir.join(format!("{}.ndjson", maps::record_type(model))))
    };
    match cmd {
        Dhis2ExtractCommand::Locations(args) => {
            let out = file(&args.out, maps::ORG_UNIT_MODEL)?;
            locations::run_locations(&args.clone().into_command(out))
        }
        Dhis2ExtractCommand::Measures(args) => {
            let out = file(&args.out, maps::INDICATOR_MODEL)?;
            measures::run_measures(&args.clone().into_command(out))
        }
        Dhis2ExtractCommand::Reports(args) => {
            let out = file(&args.out, maps::VALUE_MODEL)?;
            let org_units = match &args.org_units {
                Some(p) => p.clone(),
                None => org_units_in(&args.out).ok_or_else(|| {
                    PipesError::Usage(format!(
                        "dhis2 extract reports needs the org units: there is no {name}.ndjson in \
                         {dir}. Run `fhir-pipes dhis2 extract locations --out {dir}` first, or give \
                         --org-units FILE",
                        name = maps::record_type(maps::ORG_UNIT_MODEL),
                        dir = args.out.display()
                    ))
                })?,
            };
            reports::run_reports(&(**args).clone().into_command(out, org_units))
        }
    }
}

/// The `DHIS2OrgUnit` records in `dir`, as `dhis2 extract locations` wrote
/// them there: `DHIS2OrgUnit.ndjson`, else `DHIS2OrgUnit.ndjson.gz`.
fn org_units_in(dir: &Path) -> Option<PathBuf> {
    let name = maps::record_type(maps::ORG_UNIT_MODEL);
    [format!("{name}.ndjson"), format!("{name}.ndjson.gz")]
        .into_iter()
        .map(|f| dir.join(f))
        .find(|p| p.is_file())
}

/// True for a name made of ASCII letters, digits and hyphens, 1–`max` long.
pub fn valid_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The short name an instance is filed under. Ids start with it. The URLs
/// `dhis2 reports` reads from earlier output (the org unit identifier
/// system of `--locations`, the Measure URL and period extension of
/// `--previous`) come from the package's parameters `base` and
/// `periodExtension`, as its maps write them.
#[derive(Debug, Clone, Default)]
pub struct Instance {
    pub name: String,
    /// The package's `base`: `None` for `dhis2 extract`, which has no
    /// package, or a package without that parameter.
    base: Option<String>,
    /// The package's `periodExtension`.
    period_extension: Option<String>,
}

impl Instance {
    pub fn new(name: &str) -> Result<Self> {
        if !valid_name(name, MAX_INSTANCE_LEN) {
            return Err(PipesError::Usage(format!(
                "--instance must be letters, digits and hyphens, at most {MAX_INSTANCE_LEN} characters, got {name:?}"
            )));
        }
        Ok(Self {
            name: name.to_string(),
            ..Self::default()
        })
    }

    /// The URLs, from the parameters of the package `map` belongs to.
    pub fn with_package(mut self, map: Option<&RecordMap>) -> Self {
        if let Some(m) = map {
            self.base = m
                .parameter("base")
                .map(|b| b.trim_end_matches('/').to_string());
            self.period_extension = m.parameter("periodExtension").map(str::to_string);
        }
        self
    }

    fn parameter<'a>(value: &'a Option<String>, name: &str, what: &str) -> Result<&'a str> {
        value.as_deref().ok_or_else(|| {
            PipesError::Usage(format!(
                "{what} needs the package's string parameter {name}, which this package does \
                 not declare"
            ))
        })
    }

    /// The identifier system of the org unit UIDs on the Locations the
    /// package writes: `<base>/identifiers/dhis2/<instance>/orgunit`.
    pub fn orgunit_system(&self) -> Result<String> {
        let base = Self::parameter(&self.base, "base", "dhis2 reports --locations")?;
        Ok(format!("{base}/identifiers/dhis2/{}/orgunit", self.name))
    }

    pub fn measure_id(&self, indicator: &str) -> String {
        format!("{}-{indicator}", self.name)
    }

    /// The canonical URL of a Measure: `<base>/Measure/<id>`.
    pub fn measure_url(&self, indicator: &str) -> Result<String> {
        let base = Self::parameter(&self.base, "base", "dhis2 reports --previous")?;
        Ok(format!("{base}/Measure/{}", self.measure_id(indicator)))
    }

    /// The extension of a MeasureReport that holds the DHIS2 period.
    pub fn period_extension(&self) -> Result<&str> {
        Self::parameter(
            &self.period_extension,
            "periodExtension",
            "dhis2 reports --previous",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_short_and_plain() {
        assert!(Instance::new("sl-demo").is_ok());
        assert!(Instance::new("").is_err());
        assert!(Instance::new("sl demo").is_err());
        assert!(Instance::new("sl_demo").is_err());
        assert!(Instance::new("a-very-long-instance-name").is_err());
    }

    #[test]
    fn systems_and_ids_carry_the_instance() {
        let map = maps::icr_package(maps::VALUE_MODEL);
        let i = Instance::new("sl-demo").unwrap().with_package(Some(&map));
        assert_eq!(
            i.orgunit_system().unwrap(),
            "https://icr.healthcampaigns.org/identifiers/dhis2/sl-demo/orgunit"
        );
        assert_eq!(i.measure_id("Uvn6LCg7dVU"), "sl-demo-Uvn6LCg7dVU");
        assert_eq!(
            i.measure_url("Uvn6LCg7dVU").unwrap(),
            "https://icr.healthcampaigns.org/Measure/sl-demo-Uvn6LCg7dVU"
        );
        assert_eq!(
            i.period_extension().unwrap(),
            "https://icr.healthcampaigns.org/StructureDefinition/dhis2-period"
        );
        // dhis2 extract has no package, and needs none of them.
        let bare = Instance::new("sl-demo").unwrap();
        assert!(bare.orgunit_system().is_err());
        assert!(bare.measure_url("x").is_err());
    }
}
