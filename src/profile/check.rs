//! `fhir-pipes profile check`: compare a profile with the implementation guide
//! it was written for, so drift between the two shows up as an error rather
//! than as an empty column.
//!
//! The IG is read from a FHIR package (`package.tgz`, a path or an https URL)
//! or a directory of JSON resources (SUSHI's `fsh-generated/resources`, the IG
//! publisher's `output`, an unpacked package). Only its StructureDefinitions,
//! CodeSystems and ValueSets are used.
//!
//! What is checked, for every URL in the profile that falls under the IG's
//! canonical:
//! - an extension exists, applies to the resource the profile reads it from,
//!   and has the value type the column expects (a `valueCode` column must not
//!   name a `system`; a CodeableConcept one must, from the bound ValueSet);
//! - a code system exists, and the codes the profile names are in it;
//! - a `meta.profile` StructureDefinition exists and constrains the right type;
//! - an identifier system is sliced in one of the IG's profiles of that type.
//!
//! URLs outside the canonical (HL7, another IG) are listed as not checked.
//! Extensions and identifier slices the IG's Location and Organization
//! profiles declare but the profile does not read are listed too.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::error::{PipesError, Result};
use crate::profile::{Column, Kind, Profile};

/// The parts of an IG the check reads.
#[derive(Debug, Default)]
pub struct Ig {
    /// The package name and version, or the path, for messages.
    pub label: String,
    /// `https://example.org`: URLs under it are the IG's own.
    pub canonical: Option<String>,
    structure_definitions: HashMap<String, Value>,
    code_systems: HashMap<String, Value>,
    value_sets: HashMap<String, Value>,
}

impl Ig {
    /// Read the IG `spec` names: an https URL or path to a package `.tgz`, or
    /// a directory of JSON resources.
    pub fn load(spec: &str) -> Result<Self> {
        let mut ig = Ig {
            label: spec.to_string(),
            ..Ig::default()
        };
        if spec.starts_with("http://") {
            return Err(PipesError::Usage(format!(
                "--ig {spec}: packages are fetched over https only"
            )));
        }
        if spec.starts_with("https://") {
            ig.read_package(&fetch(spec)?, spec)?;
        } else {
            let path = Path::new(spec);
            if path.is_dir() {
                ig.read_dir(path)?;
            } else if path.is_file() {
                let bytes = std::fs::read(path).map_err(|e| PipesError::io(path, e))?;
                ig.read_package(&bytes, spec)?;
            } else {
                return Err(PipesError::Usage(format!(
                    "--ig {spec}: not a file, directory or https URL"
                )));
            }
        }
        if ig.structure_definitions.is_empty() && ig.code_systems.is_empty() {
            return Err(PipesError::Usage(format!(
                "--ig {spec}: no StructureDefinitions or CodeSystems found"
            )));
        }
        Ok(ig)
    }

    /// A gzipped tar of JSON files, as the IG publisher writes `package.tgz`.
    fn read_package(&mut self, bytes: &[u8], spec: &str) -> Result<()> {
        let bad =
            |e: std::io::Error| PipesError::Usage(format!("--ig {spec}: not a FHIR package: {e}"));
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in archive.entries().map_err(bad)? {
            let mut entry = entry.map_err(bad)?;
            let name = entry.path().map_err(bad)?.to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                self.add(&name, &text);
            }
        }
        Ok(())
    }

    fn read_dir(&mut self, dir: &Path) -> Result<()> {
        let entries = std::fs::read_dir(dir).map_err(|e| PipesError::io(dir, e))?;
        for entry in entries {
            let path = entry.map_err(|e| PipesError::io(dir, e))?.path();
            if path.is_dir() {
                self.read_dir(&path)?;
            } else if path.extension().is_some_and(|e| e == "json")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                self.add(&path.to_string_lossy(), &text);
            }
        }
        Ok(())
    }

    fn add(&mut self, name: &str, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if name.ends_with("package.json") && v.get("resourceType").is_none() {
            if let Some(c) = v.get("canonical").and_then(Value::as_str) {
                self.canonical = Some(c.trim_end_matches('/').to_string());
            }
            if let (Some(n), Some(ver)) = (
                v.get("name").and_then(Value::as_str),
                v.get("version").and_then(Value::as_str),
            ) {
                self.label = format!("{n} {ver}");
            }
            return;
        }
        let url = v.get("url").and_then(Value::as_str).map(str::to_string);
        match (v.get("resourceType").and_then(Value::as_str), url) {
            (Some("StructureDefinition"), Some(u)) => {
                self.structure_definitions.insert(u, v);
            }
            (Some("CodeSystem"), Some(u)) => {
                self.code_systems.insert(u, v);
            }
            (Some("ValueSet"), Some(u)) => {
                self.value_sets.insert(u, v);
            }
            (Some("ImplementationGuide"), Some(u)) if self.canonical.is_none() => {
                if let Some((base, _)) = u.split_once("/ImplementationGuide/") {
                    self.canonical = Some(base.to_string());
                }
            }
            _ => {}
        }
    }

    /// The IG's own URL: under its canonical, or any URL when that is unknown.
    fn owns(&self, url: &str) -> bool {
        match &self.canonical {
            Some(c) => url.starts_with(&format!("{c}/")),
            None => true,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{}{}: {} StructureDefinitions, {} CodeSystems, {} ValueSets",
            self.label,
            self.canonical
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default(),
            self.structure_definitions.len(),
            self.code_systems.len(),
            self.value_sets.len()
        )
    }

    /// Identifier systems sliced in the IG's profiles of `resource_type`.
    fn identifier_systems(&self, resource_type: &str) -> BTreeSet<String> {
        let prefix = format!("{resource_type}.identifier:");
        self.profiles_of(resource_type)
            .flat_map(elements)
            .filter(|e| {
                e.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with(&prefix) && id.ends_with(".system"))
            })
            .filter_map(|e| {
                ["patternUri", "fixedUri"]
                    .iter()
                    .find_map(|k| e.get(*k).and_then(Value::as_str))
            })
            .map(str::to_string)
            .collect()
    }

    /// Extensions the IG's profiles of `resource_type` slice in.
    fn profile_extensions(&self, resource_type: &str) -> BTreeSet<String> {
        let prefix = format!("{resource_type}.extension:");
        self.profiles_of(resource_type)
            .flat_map(elements)
            .filter(|e| {
                e.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with(&prefix) && !id[prefix.len()..].contains('.'))
            })
            .flat_map(|e| {
                e.get("type")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|t| {
                        t.get("profile")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter_map(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    /// The IG's constraining StructureDefinitions of `resource_type`.
    fn profiles_of<'a>(&'a self, resource_type: &'a str) -> impl Iterator<Item = &'a Value> {
        self.structure_definitions.values().filter(move |sd| {
            sd.get("type").and_then(Value::as_str) == Some(resource_type)
                && sd.get("derivation").and_then(Value::as_str) == Some("constraint")
        })
    }
}

/// The elements of a StructureDefinition: its snapshot, else its differential.
fn elements(sd: &Value) -> impl Iterator<Item = &Value> {
    let list = |k: &str| {
        sd.get(k)
            .and_then(|s| s.get("element"))
            .and_then(Value::as_array)
    };
    list("snapshot")
        .or_else(|| list("differential"))
        .into_iter()
        .flatten()
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let client = crate::extract::client::FhirClient::new(
        crate::extract::client::Auth::None,
        crate::cli::DEFAULT_RETRIES,
        Duration::from_secs(120),
    )?;
    client
        .get(url)
        .map(|f| f.body)
        .map_err(|e| PipesError::Environment(format!("--ig {url}: {e}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Where in the profile: `[[location.column]] settlement_type`.
    pub place: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct CheckResult {
    pub findings: Vec<Finding>,
    /// URLs the check cannot judge, with the reason.
    pub not_checked: BTreeMap<String, String>,
    /// Extensions and identifier systems the IG's Location and Organization
    /// profiles declare that the profile does not read, by resource type.
    pub not_read: Vec<(String, Vec<String>)>,
}

impl CheckResult {
    pub fn errors(&self) -> usize {
        self.count(Severity::Error)
    }

    pub fn warnings(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, s: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == s).count()
    }
}

/// The value an extension column or role expects.
#[derive(Clone, Copy)]
enum Expect<'a> {
    Any,
    Code,
    Concept(&'a str),
    Attachment,
    Reference,
}

struct Checker<'a> {
    ig: &'a Ig,
    out: CheckResult,
}

impl Checker<'_> {
    fn outside(&mut self, url: &str) {
        self.out
            .not_checked
            .insert(url.to_string(), "outside the IG".into());
    }

    fn finding(&mut self, severity: Severity, place: &str, message: String) {
        self.out.findings.push(Finding {
            severity,
            place: place.to_string(),
            message,
        });
    }

    fn extension(&mut self, place: &str, url: &str, on: &[&str], want: Expect) {
        let Some(sd) = self.ig.structure_definitions.get(url) else {
            if self.ig.owns(url) {
                self.finding(
                    Severity::Error,
                    place,
                    format!("{url} is not an extension in the IG"),
                );
            } else {
                self.outside(url);
            }
            return;
        };
        if sd.get("type").and_then(Value::as_str) != Some("Extension") {
            self.finding(Severity::Error, place, format!("{url} is not an extension"));
            return;
        }
        let contexts: Vec<&str> = sd
            .get("context")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("expression").and_then(Value::as_str))
            .collect();
        let applies = contexts.is_empty()
            || contexts.iter().any(|c| {
                matches!(*c, "Element" | "Resource" | "DomainResource")
                    || on.iter().any(|r| c == r || c.starts_with(&format!("{r}.")))
            });
        if !applies {
            self.finding(
                Severity::Error,
                place,
                format!(
                    "{url} applies to {}, not {}",
                    contexts.join(", "),
                    on.join(" or ")
                ),
            );
        }
        let Some(value) = elements(sd).find(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == "Extension.value[x]")
        }) else {
            return;
        };
        let types: Vec<&str> = if value.get("max").and_then(Value::as_str) == Some("0") {
            Vec::new()
        } else {
            value
                .get("type")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|t| t.get("code").and_then(Value::as_str))
                .collect()
        };
        let complex = value.get("max").and_then(Value::as_str) == Some("0");
        if types.is_empty() && !complex {
            // A differential that does not constrain the value: nothing to say.
            return;
        }
        let shown = if complex {
            "no value (a complex extension)".to_string()
        } else {
            types.join(" or ")
        };
        let concept = types
            .iter()
            .any(|t| matches!(*t, "CodeableConcept" | "Coding"));
        let bound = self.bound_systems(value);
        match want {
            Expect::Any => {}
            Expect::Code if !types.contains(&"code") => {
                let hint = match (concept, bound.first()) {
                    (true, Some(s)) => format!("; add system = \"{s}\""),
                    (true, None) => "; add the code system as `system`".into(),
                    _ => String::new(),
                };
                self.finding(
                    Severity::Error,
                    place,
                    format!("{url} has value {shown}, not code{hint}"),
                );
            }
            Expect::Code => {}
            Expect::Concept(system) => {
                if !concept {
                    let hint = if types.contains(&"code") {
                        "; remove `system`"
                    } else {
                        ""
                    };
                    self.finding(
                        Severity::Error,
                        place,
                        format!("{url} has value {shown}, not CodeableConcept{hint}"),
                    );
                } else if !bound.is_empty() && !bound.iter().any(|s| s == system) {
                    self.finding(
                        Severity::Warning,
                        place,
                        format!(
                            "{system} is not in the ValueSet {url} is bound to ({})",
                            bound.join(", ")
                        ),
                    );
                }
            }
            Expect::Attachment if !types.contains(&"Attachment") => self.finding(
                Severity::Warning,
                place,
                format!("{url} has value {shown}, not Attachment"),
            ),
            Expect::Reference if !types.contains(&"Reference") => self.finding(
                Severity::Warning,
                place,
                format!("{url} has value {shown}, not Reference"),
            ),
            Expect::Attachment | Expect::Reference => {}
        }
    }

    /// The code systems of the IG ValueSet an element is bound to.
    fn bound_systems(&self, element: &Value) -> Vec<String> {
        let Some(vs) = element
            .get("binding")
            .and_then(|b| b.get("valueSet"))
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        let vs = vs.split('|').next().unwrap_or(vs);
        self.ig
            .value_sets
            .get(vs)
            .and_then(|v| v.get("compose"))
            .and_then(|c| c.get("include"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|i| i.get("system").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    /// A code system the profile names, and the codes it says are in it.
    /// `missing` is how serious its absence is.
    fn code_system(&mut self, place: &str, system: &str, codes: &[&str], missing: Severity) {
        let Some(cs) = self.ig.code_systems.get(system) else {
            if self.ig.owns(system) {
                self.finding(
                    missing,
                    place,
                    format!("{system} is not a CodeSystem in the IG"),
                );
            } else {
                self.outside(system);
            }
            return;
        };
        let have = concept_codes(cs);
        for code in codes {
            if !have.contains(*code) {
                self.finding(
                    Severity::Error,
                    place,
                    format!("`{code}` is not a code in {system}"),
                );
            }
        }
    }

    fn structure(&mut self, place: &str, url: &str, resource_type: &str) {
        match self.ig.structure_definitions.get(url) {
            None if self.ig.owns(url) => self.finding(
                Severity::Error,
                place,
                format!("{url} is not a profile in the IG"),
            ),
            None => {
                self.outside(url);
            }
            Some(sd) => {
                let ty = sd.get("type").and_then(Value::as_str).unwrap_or("?");
                if ty != resource_type {
                    self.finding(
                        Severity::Error,
                        place,
                        format!("{url} constrains {ty}, not {resource_type}"),
                    );
                }
            }
        }
    }

    fn identifier_system(&mut self, place: &str, system: &str, resource_type: &str) {
        let sliced = self.ig.identifier_systems(resource_type);
        if !self.ig.owns(system) {
            self.outside(system);
        } else if sliced.is_empty() {
            self.out.not_checked.insert(
                system.to_string(),
                format!("the IG's {resource_type} profiles slice no identifier systems"),
            );
        } else if !sliced.contains(system) {
            self.finding(
                Severity::Warning,
                place,
                format!(
                    "{system} is not an identifier slice in any of the IG's {resource_type} profiles"
                ),
            );
        }
    }

    fn column(&mut self, section: &str, resource_type: &str, c: &Column) {
        let place = format!("[[{section}.column]] {}", c.name);
        match c.kind {
            Kind::Identifier => {
                for s in &c.systems {
                    self.identifier_system(&place, s, resource_type);
                }
            }
            Kind::ExtensionCode => {
                let url = c.url.as_deref().unwrap_or_default();
                let want = match c.system.as_deref() {
                    Some(s) => Expect::Concept(s),
                    None => Expect::Code,
                };
                self.extension(&place, url, &[resource_type], want);
                if let Some(s) = c.system.as_deref() {
                    self.code_system(&place, s, &[], Severity::Error);
                }
            }
            Kind::TypeCoding | Kind::TypeText => {
                let system = c.system.as_deref().unwrap_or_default();
                self.code_system(&place, system, &[], Severity::Error);
            }
        }
    }
}

/// Every code in a CodeSystem, nested concepts included.
fn concept_codes(cs: &Value) -> BTreeSet<String> {
    fn walk(concepts: Option<&Value>, out: &mut BTreeSet<String>) {
        for c in concepts.and_then(Value::as_array).into_iter().flatten() {
            if let Some(code) = c.get("code").and_then(Value::as_str) {
                out.insert(code.to_string());
            }
            walk(c.get("concept"), out);
        }
    }
    let mut out = BTreeSet::new();
    walk(cs.get("concept"), &mut out);
    out
}

/// Check `profile` against `ig`.
pub fn check(profile: &Profile, ig: &Ig) -> CheckResult {
    let mut c = Checker {
        ig,
        out: CheckResult::default(),
    };
    let loc = &profile.location;
    for url in &loc.boundary_extensions {
        c.extension(
            "[location] boundary_extensions",
            url,
            &["Location"],
            Expect::Attachment,
        );
    }
    if let Some(url) = &loc.overlays_extension {
        c.extension(
            "[location] overlays_extension",
            url,
            &["Location"],
            Expect::Reference,
        );
    }
    if let Some(url) = &loc.spatial_index_extension {
        c.extension(
            "[location] spatial_index_extension",
            url,
            &["Location"],
            Expect::Any,
        );
    }
    let type_codes: Vec<&str> = loc
        .admin_unit_type
        .iter()
        .chain(&profile.organization.facility_type)
        .map(String::as_str)
        .collect();
    for (i, system) in loc.type_systems.iter().enumerate() {
        // The first is the IG's; later ones are older URIs data may carry.
        let (codes, missing) = if i == 0 {
            (type_codes.as_slice(), Severity::Error)
        } else {
            (&[][..], Severity::Warning)
        };
        c.code_system("[location] type_systems", system, codes, missing);
    }
    for col in &loc.columns {
        c.column("location", "Location", col);
    }
    for col in &profile.organization.columns {
        c.column("organization", "Organization", col);
    }
    if let Some(d) = &profile.dhis2 {
        c.structure("[dhis2] location_profile", &d.location_profile, "Location");
        c.structure(
            "[dhis2] facility_organization_profile",
            &d.facility_organization_profile,
            "Organization",
        );
        c.code_system(
            "[dhis2] location_type_system",
            &d.location_type_system,
            &type_codes,
            Severity::Error,
        );
        c.code_system(
            "[dhis2] project_tag_system",
            &d.project_tag_system,
            &[],
            Severity::Error,
        );
        c.identifier_system(
            "[dhis2] country_code_system",
            &d.country_code_system,
            "Location",
        );
        for (key, url) in [
            ("period_extension", &d.period_extension),
            ("campaign_extension", &d.campaign_extension),
            ("source_extension", &d.source_extension),
        ] {
            c.extension(
                &format!("[dhis2] {key}"),
                url,
                &["MeasureReport"],
                Expect::Any,
            );
        }
        c.extension(
            "[dhis2] indicator_extension",
            &d.indicator_extension,
            &["Measure"],
            Expect::Any,
        );
    }
    if let Some(p) = &profile.population {
        c.structure("[population] group_profile", &p.group_profile, "Group");
        c.code_system(
            "[population] characteristic_system",
            &p.characteristic_system,
            &[],
            Severity::Error,
        );
        let known: Vec<&str> = p.known_source_codes.iter().map(String::as_str).collect();
        c.code_system(
            "[population] known_source_codes",
            &p.denominator_source_system,
            &known,
            Severity::Error,
        );
        if let Some(cs) = ig.code_systems.get(&p.denominator_source_system) {
            let extra: Vec<String> = concept_codes(cs)
                .into_iter()
                .filter(|code| !p.known_source_codes.contains(code))
                .collect();
            if !extra.is_empty() {
                c.finding(
                    Severity::Warning,
                    "[population] known_source_codes",
                    format!(
                        "the IG's {} also has {}",
                        p.denominator_source_system,
                        extra.join(", ")
                    ),
                );
            }
        }
        for (key, url) in [
            (
                "denominator_source_extension",
                &p.denominator_source_extension,
            ),
            ("denominator_type_extension", &p.denominator_type_extension),
            ("estimate_date_extension", &p.estimate_date_extension),
            (
                "is_planning_denominator_extension",
                &p.is_planning_denominator_extension,
            ),
            ("is_calculated_extension", &p.is_calculated_extension),
        ] {
            c.extension(&format!("[population] {key}"), url, &["Group"], Expect::Any);
        }
    }
    c.out.not_read = not_read(profile, ig);
    c.out
}

/// What the IG's Location and Organization profiles declare that `profile`
/// does not read.
fn not_read(profile: &Profile, ig: &Ig) -> Vec<(String, Vec<String>)> {
    let loc = &profile.location;
    let mut read: BTreeSet<&str> = loc
        .boundary_extensions
        .iter()
        .chain(&loc.overlays_extension)
        .chain(&loc.spatial_index_extension)
        .map(String::as_str)
        .collect();
    for col in loc.columns.iter().chain(&profile.organization.columns) {
        read.extend(col.url.as_deref());
        read.extend(col.systems.iter().map(String::as_str));
    }
    if let Some(d) = &profile.dhis2 {
        read.insert(&d.country_code_system);
    }
    let mut out = Vec::new();
    for ty in ["Location", "Organization"] {
        let missing: Vec<String> = ig
            .profile_extensions(ty)
            .into_iter()
            .chain(ig.identifier_systems(ty))
            .filter(|u| !read.contains(u.as_str()))
            .collect();
        if !missing.is_empty() {
            out.push((ty.to_string(), missing));
        }
    }
    out
}

/// Print the result. Errors make the command fail.
pub fn run(args: &crate::cli::ProfileCheckArgs, spec: Option<&str>) -> Result<()> {
    let profile = Profile::load(spec)?;
    if profile.source == "no profile" {
        return Err(PipesError::Usage(
            "profile check needs a profile: pass --profile FILE|URL".into(),
        ));
    }
    let ig = Ig::load(&args.ig)?;
    println!("profile: {} ({})", profile.name, profile.source);
    println!("ig: {}", ig.summary());
    let result = check(&profile, &ig);
    let mut findings = result.findings.clone();
    findings.sort_by_key(|f| f.severity);
    for f in &findings {
        let level = match f.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        println!("{level}: {}: {}", f.place, f.message);
    }
    if !result.not_checked.is_empty() {
        println!("not checked:");
        for (url, why) in &result.not_checked {
            println!("  {url} ({why})");
        }
    }
    for (ty, urls) in &result.not_read {
        println!("in the IG's {ty} profiles but not read by the profile:");
        for url in urls {
            println!("  {url}");
        }
    }
    println!(
        "{} error(s), {} warning(s)",
        result.errors(),
        result.warnings()
    );
    if result.errors() > 0 {
        return Err(PipesError::Findings(format!(
            "the profile does not match the IG: {} error(s)",
            result.errors()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package.tgz holding `files` under `package/`.
    fn package(files: &[(&str, &str)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (name, text) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("package/{name}"), text.as_bytes())
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_package_gives_its_canonical_name_and_resources() {
        let bytes = package(&[
            (
                "package.json",
                r#"{"name":"example.ig","version":"1.2.0","canonical":"https://example.org/"}"#,
            ),
            (
                "CodeSystem-kinds.json",
                r#"{"resourceType":"CodeSystem","url":"https://example.org/CodeSystem/kinds","concept":[{"code":"a","concept":[{"code":"b"}]}]}"#,
            ),
            ("other.txt", "ignored"),
        ]);
        let mut ig = Ig::default();
        ig.read_package(&bytes, "p.tgz").unwrap();
        assert_eq!(ig.label, "example.ig 1.2.0");
        assert_eq!(ig.canonical.as_deref(), Some("https://example.org"));
        let cs = &ig.code_systems["https://example.org/CodeSystem/kinds"];
        assert_eq!(
            concept_codes(cs).into_iter().collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(ig.owns("https://example.org/StructureDefinition/x"));
        assert!(!ig.owns("http://hl7.org/fhir/StructureDefinition/x"));
    }

    #[test]
    fn not_a_package_is_a_usage_error() {
        let mut ig = Ig::default();
        let err = ig.read_package(b"not gzip", "p.tgz").unwrap_err();
        assert!(err.to_string().contains("not a FHIR package"), "{err}");
    }
}
