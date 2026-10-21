//! Profiles: the implementation-guide-specific parts of the Location, DHIS2 and
//! population commands, in a TOML file instead of the code.
//!
//! Without a profile only base FHIR is read and written. A profile adds the
//! IG's extra Location and Organization columns (each with a rule saying where
//! its value lives, which `diff` follows back to write an edit), the roles some
//! type codes and extensions play (admin units, the admin code, the spatial
//! index), and the URLs the DHIS2 and population commands mint. See
//! docs/profiles.md; examples/profiles/icr.toml is a complete one.
//!
//! `--profile` (or FHIR_PIPES_PROFILE) takes a file path or an https URL.
//! `transform locations` and `run` keep a copy beside the snapshot, which a
//! later `diff` uses when no profile is given.

pub mod check;

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{PipesError, Result};
use crate::fhir::location::{Identifier, coding_code_by_system};
use crate::fhir::organization::concept_text_by_system;

/// The copy of the profile kept in a snapshot directory.
pub const SNAPSHOT_COPY: &str = "profile.toml";

/// What `--profile` names when it is not given: base FHIR only.
pub const BASE_TEXT: &str = "name = \"base\"\n";
/// `Profile::source` when no profile was given.
pub const NO_PROFILE: &str = "no profile";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// A short name for messages (`icr`). Not otherwise used.
    #[serde(default = "base_name")]
    pub name: String,
    #[serde(default)]
    pub location: LocationProfile,
    #[serde(default)]
    pub organization: OrganizationProfile,
    pub dhis2: Option<Dhis2Profile>,
    pub population: Option<PopulationProfile>,
    /// Where it came from: a path, a URL, or [`NO_PROFILE`].
    #[serde(skip)]
    pub source: String,
    /// The file's text, as kept beside a snapshot.
    #[serde(skip)]
    pub text: String,
}

fn base_name() -> String {
    "base".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationProfile {
    /// Boundary attachment extensions tried before the HL7 one; diff writes a
    /// new boundary to the first, or to the HL7 one when there are none.
    #[serde(default)]
    pub boundary_extensions: Vec<String>,
    /// valueReference list of admin units an operational area overlaps.
    pub overlays_extension: Option<String>,
    /// valueReference to what a catchment area is the catchment of: the
    /// `catchment_of` of `refresh`'s catchment tiles.
    pub catchment_extension: Option<String>,
    /// The spatial index extension (quadkey, geohash, h3 cells).
    pub spatial_index_extension: Option<String>,
    /// The Location type code of an administrative unit.
    pub admin_unit_type: Option<String>,
    /// Code systems of the Location type codes: `population --type` matches a
    /// code under any of them, wherever it sits in `type`. Empty: any system.
    #[serde(default)]
    pub type_systems: Vec<String>,
    /// The Location column holding an admin unit's code.
    pub admin_code_column: Option<String>,
    /// `meta.profile` of the Locations `import` writes; else `[dhis2]
    /// location_profile`.
    pub profile: Option<String>,
    /// The identifier system of the admin codes `import admin` writes; else
    /// `[dhis2] country_code_system`.
    pub admin_code_system: Option<String>,
    #[serde(default, rename = "column")]
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationProfile {
    /// A created Location of this type with no managingOrganization gets one.
    pub facility_type: Option<String>,
    /// Prefix of the id of an Organization diff creates (`org-`).
    #[serde(default = "default_org_prefix")]
    pub id_prefix: String,
    /// `meta.profile` of the facility Organizations `import sites` writes;
    /// else `[dhis2] facility_organization_profile`.
    pub profile: Option<String>,
    #[serde(default, rename = "column")]
    pub columns: Vec<Column>,
}

fn default_org_prefix() -> String {
    "org-".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dhis2Profile {
    pub base: String,
    pub project_tag_system: String,
    pub location_profile: String,
    pub facility_organization_profile: String,
    pub location_type_system: String,
    /// The identifier system `dhis2 locations --country-code` writes on the
    /// top org unit (it should be one the admin code column reads).
    pub country_code_system: String,
    pub period_extension: String,
    pub campaign_extension: String,
    /// Where each MeasureReport came from: a CodeableConcept under
    /// `source_system`, `dhis2-analytics` or `dhis2-data-values`.
    pub source_extension: String,
    pub source_system: String,
    pub indicator_extension: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PopulationProfile {
    pub group_profile: String,
    pub characteristic_system: String,
    pub denominator_source_system: String,
    pub denominator_source_extension: String,
    pub denominator_type_extension: String,
    pub estimate_date_extension: String,
    pub is_planning_denominator_extension: String,
    pub is_calculated_extension: String,
    #[serde(default)]
    pub known_source_codes: Vec<String>,
    /// Display text for source codes, e.g. worldpop = "WorldPop modelled estimate".
    #[serde(default)]
    pub source_displays: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Identifier,
    ExtensionCode,
    TypeCoding,
    TypeText,
}

/// One extra column. Which of `systems`, `url` and `system` it needs depends
/// on `kind`; `check` refuses a column that has the wrong ones.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub systems: Vec<String>,
    pub url: Option<String>,
    pub system: Option<String>,
    /// A Location type_coding column diff also writes to the paired
    /// Organization's type.
    #[serde(default)]
    pub mirror_to_organization: bool,
}

impl Column {
    /// The value in `resource` (whose identifiers are already parsed).
    pub fn read(
        &self,
        resource: &Map<String, Value>,
        identifiers: &[Identifier],
    ) -> Option<String> {
        match self.kind {
            Kind::Identifier => self.systems.iter().find_map(|system| {
                identifiers
                    .iter()
                    .find(|i| i.system.as_deref() == Some(system.as_str()))
                    .and_then(|i| i.value.clone())
            }),
            Kind::ExtensionCode => {
                let url = self.url.as_deref()?;
                let ext = resource
                    .get("extension")?
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_object)
                    .find(|e| e.get("url").and_then(Value::as_str) == Some(url))?;
                match self.system.as_deref() {
                    // A CodeableConcept extension: the code under `system`.
                    Some(system) => coding_code_by_system(ext.get("valueCodeableConcept"), system),
                    None => ext
                        .get("valueCode")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }
            }
            Kind::TypeCoding => {
                coding_code_by_system(resource.get("type"), self.system.as_deref()?)
            }
            Kind::TypeText => concept_text_by_system(resource.get("type"), self.system.as_deref()?),
        }
    }
}

impl Default for Profile {
    fn default() -> Self {
        Profile::base()
    }
}

impl Profile {
    /// No profile: base FHIR only.
    pub fn base() -> Self {
        let mut p: Profile = toml::from_str(BASE_TEXT).expect("the base profile parses");
        p.source = NO_PROFILE.into();
        p.text = BASE_TEXT.into();
        p
    }

    /// True when no profile was given: base FHIR only.
    pub fn is_base(&self) -> bool {
        self.source == NO_PROFILE
    }

    /// The end of a message saying the profile lacks `what` (such as
    /// "[dhis2] section"): that no profile was given, or which profile
    /// lacks it, and what to do.
    pub fn lacks(&self, what: &str) -> String {
        if self.is_base() {
            "no profile was given. Pass one with --profile; see docs/profiles.md".into()
        } else {
            format!(
                "the profile {} has no {what}. Pass a profile that has it; see docs/profiles.md",
                self.source
            )
        }
    }

    /// Parse and check a profile's text; `source` is for messages.
    pub fn parse(text: &str, source: &str) -> Result<Self> {
        let mut p: Profile = toml::from_str(text)
            .map_err(|e| PipesError::Usage(format!("profile {source}: {e}")))?;
        p.source = source.to_string();
        p.text = text.to_string();
        p.check()
            .map_err(|e| PipesError::Usage(format!("profile {source}: {e}")))?;
        Ok(p)
    }

    /// The profile `spec` names: an https URL, a file path, or none (base).
    pub fn load(spec: Option<&str>) -> Result<Self> {
        let Some(spec) = spec.filter(|s| !s.is_empty()) else {
            return Ok(Profile::base());
        };
        if spec.starts_with("http://") {
            return Err(PipesError::Usage(format!(
                "--profile {spec}: profiles are fetched over https only"
            )));
        }
        let text = if spec.starts_with("https://") {
            fetch(spec)?
        } else {
            std::fs::read_to_string(spec)
                .map_err(|e| PipesError::Usage(format!("--profile {spec}: {e}")))?
        };
        Profile::parse(&text, spec)
    }

    /// The profile for a command that works from a snapshot: `spec` if given,
    /// else the copy `transform locations` left in `snapshot`, else base. When
    /// `spec` and the copy differ, says so: the snapshot's output was made
    /// with the copy.
    pub fn for_snapshot(spec: Option<&str>, snapshot: &Path) -> Result<Self> {
        let copy_path = snapshot.join(SNAPSHOT_COPY);
        let copy = std::fs::read_to_string(&copy_path).ok();
        match (spec.filter(|s| !s.is_empty()), copy) {
            (Some(spec), copy) => {
                let p = Profile::load(Some(spec))?;
                if let Some(copy) = copy
                    && copy != p.text
                {
                    eprintln!(
                        "fhir-pipes: warning: --profile {spec} differs from {}, the profile \
                         this snapshot was last transformed with; using --profile",
                        copy_path.display()
                    );
                }
                Ok(p)
            }
            (None, Some(copy)) => Profile::parse(&copy, &copy_path.display().to_string()),
            (None, None) => Ok(Profile::base()),
        }
    }

    /// Keep a copy beside the snapshot, for a later `diff`. Nothing is
    /// written when the copy is already this profile; a snapshot that cannot
    /// be written to gets a warning, not a failed transform.
    pub fn save_copy(&self, snapshot: &Path) {
        let path = snapshot.join(SNAPSHOT_COPY);
        if std::fs::read_to_string(&path).is_ok_and(|copy| copy == self.text) {
            return;
        }
        let tmp = snapshot.join(format!("{SNAPSHOT_COPY}.{}.tmp", std::process::id()));
        let written = crate::fs::AtomicFile::create_at(&path, tmp).and_then(|mut f| {
            std::io::Write::write_all(&mut f, self.text.as_bytes())
                .map_err(|e| PipesError::io(f.tmp(), e))?;
            f.commit()
        });
        if let Err(e) = written {
            // The I/O error alone: the message names the path already.
            let e = match e {
                PipesError::Io { source, .. } => source.to_string(),
                e => e.to_string(),
            };
            eprintln!(
                "fhir-pipes: warning: could not keep a copy of the profile at {} ({e}); \
                 pass --profile to diff, index and population",
                path.display()
            );
        }
    }

    /// Every extra column name, Location columns first.
    pub fn column_names(&self) -> impl Iterator<Item = &str> {
        self.location
            .columns
            .iter()
            .chain(&self.organization.columns)
            .map(|c| c.name.as_str())
    }

    /// Index of the Location column holding an admin unit's code.
    pub fn admin_code_index(&self) -> Option<usize> {
        let name = self.location.admin_code_column.as_deref()?;
        self.location.columns.iter().position(|c| c.name == name)
    }

    pub fn dhis2(&self) -> Result<&Dhis2Profile> {
        self.dhis2.as_ref().ok_or_else(|| {
            PipesError::Usage(format!(
                "dhis2 needs a profile with a [dhis2] section (the identifier systems and \
                 profiles it writes); {}",
                self.lacks("[dhis2] section")
            ))
        })
    }

    pub fn population(&self) -> Result<&PopulationProfile> {
        self.population.as_ref().ok_or_else(|| {
            PipesError::Usage(format!(
                "population needs a profile with a [population] section (the Group profile \
                 and extensions it writes); {}",
                self.lacks("[population] section")
            ))
        })
    }

    pub fn spatial_index_extension(&self) -> Result<&str> {
        self.location
            .spatial_index_extension
            .as_deref()
            .ok_or_else(|| {
                PipesError::Usage(format!(
                    "index needs a profile that names [location] spatial_index_extension; {}",
                    self.lacks("[location] spatial_index_extension")
                ))
            })
    }

    /// The boundary extension a written Location carries: the first of
    /// `[location] boundary_extensions`, else the HL7 one.
    pub fn boundary_extension(&self) -> &str {
        self.location.boundary_extensions.first().map_or(
            crate::fhir::location::HL7_BOUNDARY_EXTENSION_URL,
            String::as_str,
        )
    }

    /// `meta.profile` of written Locations: `[location] profile`, else
    /// `[dhis2] location_profile`.
    pub fn location_profile(&self) -> Option<&str> {
        self.location
            .profile
            .as_deref()
            .or(self.dhis2.as_ref().map(|d| d.location_profile.as_str()))
    }

    /// `meta.profile` of written facility Organizations: `[organization]
    /// profile`, else `[dhis2] facility_organization_profile`.
    pub fn organization_profile(&self) -> Option<&str> {
        self.organization.profile.as_deref().or(self
            .dhis2
            .as_ref()
            .map(|d| d.facility_organization_profile.as_str()))
    }

    /// The system of written Location type codes: the first of `[location]
    /// type_systems`, else `[dhis2] location_type_system`.
    pub fn location_type_system(&self) -> Option<&str> {
        self.location
            .type_systems
            .first()
            .map(String::as_str)
            .or(self.dhis2.as_ref().map(|d| d.location_type_system.as_str()))
    }

    /// The identifier system of written admin codes: `[location]
    /// admin_code_system`, else `[dhis2] country_code_system`.
    pub fn admin_code_system(&self) -> Option<&str> {
        self.location
            .admin_code_system
            .as_deref()
            .or(self.dhis2.as_ref().map(|d| d.country_code_system.as_str()))
    }

    fn check(&self) -> std::result::Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for (section, columns) in [
            ("location", &self.location.columns),
            ("organization", &self.organization.columns),
        ] {
            for c in columns {
                if !valid_column_name(&c.name) {
                    return Err(format!(
                        "[[{section}.column]] name `{}`: use lower case letters, digits and _",
                        c.name
                    ));
                }
                if crate::write::schema::is_builtin_column(&c.name) {
                    return Err(format!(
                        "[[{section}.column]] `{}` is a built-in column",
                        c.name
                    ));
                }
                if !seen.insert(c.name.as_str()) {
                    return Err(format!("column `{}` is declared twice", c.name));
                }
                let needs = match c.kind {
                    Kind::Identifier if c.systems.is_empty() => Some("systems"),
                    Kind::ExtensionCode if c.url.is_none() => Some("url"),
                    Kind::TypeCoding | Kind::TypeText if c.system.is_none() => Some("system"),
                    _ => None,
                };
                if let Some(field) = needs {
                    return Err(format!(
                        "column `{}` ({:?}) needs `{field}`",
                        c.name, c.kind
                    ));
                }
                let stray = match c.kind {
                    Kind::Identifier => c.url.is_some() || c.system.is_some(),
                    Kind::ExtensionCode => !c.systems.is_empty(),
                    Kind::TypeCoding | Kind::TypeText => !c.systems.is_empty() || c.url.is_some(),
                };
                if stray {
                    return Err(format!(
                        "column `{}` ({:?}) has a key that kind does not use",
                        c.name, c.kind
                    ));
                }
                if c.mirror_to_organization && (section != "location" || c.kind != Kind::TypeCoding)
                {
                    return Err(format!(
                        "column `{}`: mirror_to_organization is for Location type_coding columns",
                        c.name
                    ));
                }
            }
        }
        if let Some(name) = &self.location.admin_code_column
            && self.admin_code_index().is_none()
        {
            return Err(format!(
                "[location] admin_code_column `{name}` is not a [[location.column]]"
            ));
        }
        Ok(())
    }
}

fn valid_column_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn fetch(url: &str) -> Result<String> {
    let body = fetch_bytes(url, "--profile", Duration::from_secs(60))?;
    String::from_utf8(body)
        .map_err(|_| PipesError::Usage(format!("--profile {url}: not UTF-8 text")))
}

/// The body of `url`, unauthenticated, with the usual retries; an error
/// names `flag`, the option the URL came from.
pub(crate) fn fetch_bytes(url: &str, flag: &str, timeout: Duration) -> Result<Vec<u8>> {
    let client = crate::http::FhirClient::shared(
        crate::cli::DEFAULT_RETRIES,
        timeout,
        crate::http::DEFAULT_BODY_LIMIT,
    )?;
    client
        .get(url)
        .map(|f| f.body)
        .map_err(|e| PipesError::Environment(format!("{flag} {url}: {e}")))
}

#[cfg(test)]
pub(crate) fn icr_example() -> Profile {
    Profile::parse(
        include_str!("../../examples/profiles/icr.toml"),
        "examples/profiles/icr.toml",
    )
    .expect("the ICR example profile parses")
}

/// The ICR example plus two things ICR's IG does not have, so tests still
/// cover them: an overlays extension and a plain valueCode extension column.
#[cfg(test)]
pub(crate) const TEST_OVERLAYS_EXTENSION: &str =
    "https://example.org/StructureDefinition/overlays-admin-unit";
#[cfg(test)]
pub(crate) const TEST_DELIVERY_STRATEGY_EXTENSION: &str =
    "https://example.org/StructureDefinition/delivery-strategy";
#[cfg(test)]
pub(crate) fn test_profile() -> Profile {
    let mut p = icr_example();
    p.location.overlays_extension = Some(TEST_OVERLAYS_EXTENSION.into());
    p.location.columns.push(Column {
        name: "delivery_strategy".into(),
        kind: Kind::ExtensionCode,
        systems: Vec::new(),
        url: Some(TEST_DELIVERY_STRATEGY_EXTENSION.into()),
        system: None,
        mirror_to_organization: false,
    });
    p.check().expect("the test profile is valid");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icr_example_parses_with_every_section() {
        let p = icr_example();
        assert_eq!(p.name, "icr");
        assert_eq!(
            p.location
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            [
                "pcode",
                "gers_id",
                "settlement_type",
                "facility_level",
                "ownership"
            ]
        );
        assert_eq!(p.admin_code_index(), Some(0));
        assert!(p.dhis2.is_some() && p.population.is_some());
    }

    #[test]
    fn base_has_nothing_extra() {
        let p = Profile::base();
        assert_eq!(p.column_names().count(), 0);
        assert!(p.dhis2().is_err());
        assert!(p.population().is_err());
        assert!(p.spatial_index_extension().is_err());
    }

    #[test]
    fn a_missing_section_is_described_plainly() {
        let base = Profile::base();
        assert!(base.is_base());
        let err = base.dhis2().unwrap_err().to_string();
        assert!(
            err.ends_with("; no profile was given. Pass one with --profile; see docs/profiles.md"),
            "{err}"
        );
        let mut named = icr_example();
        named.dhis2 = None;
        named.source = "ig/profile.toml".into();
        assert!(!named.is_base());
        let err = named.dhis2().unwrap_err().to_string();
        assert!(
            err.ends_with(
                "; the profile ig/profile.toml has no [dhis2] section. Pass a profile that has \
                 it; see docs/profiles.md"
            ),
            "{err}"
        );
    }

    #[test]
    fn bad_profiles_are_refused() {
        for (text, why) in [
            ("nme = \"typo\"", "unknown field"),
            (
                "[[location.column]]\nname = \"x\"\nkind = \"fhirpath\"",
                "unknown variant",
            ),
            (
                "[[location.column]]\nname = \"x\"\nkind = \"identifier\"",
                "needs `systems`",
            ),
            (
                "[[location.column]]\nname = \"x\"\nkind = \"extension_code\"\nurl = \"u\"\nsystems = [\"s\"]",
                "does not use",
            ),
            (
                "[[location.column]]\nname = \"name\"\nkind = \"extension_code\"\nurl = \"u\"",
                "built-in",
            ),
            (
                "[[location.column]]\nname = \"Bad\"\nkind = \"extension_code\"\nurl = \"u\"",
                "lower case",
            ),
            (
                "[location]\nadmin_code_column = \"pcode\"",
                "is not a [[location.column]]",
            ),
            (
                "[[organization.column]]\nname = \"x\"\nkind = \"type_coding\"\nsystem = \"s\"\nmirror_to_organization = true",
                "mirror_to_organization",
            ),
        ] {
            let err = Profile::parse(text, "t.toml").unwrap_err().to_string();
            assert!(err.contains(why), "{text}: {err}");
        }
    }

    #[test]
    fn columns_read_their_kind() {
        let p = icr_example();
        let obj: Value = serde_json::json!({
            "resourceType": "Location",
            "type": [{"coding": [
                {"system": "x", "code": "facility"},
                {"system": "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs", "code": "public"}
            ]}],
            "extension": [{"url": "https://icr.healthcampaigns.org/StructureDefinition/settlement-type",
                "valueCodeableConcept": {"coding": [
                    {"system": "https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs", "code": "urban"}]}}],
        });
        let ids = vec![Identifier {
            system: Some("https://icr.healthcampaigns.org/identifiers/national-admin-code".into()),
            value: Some("NG".into()),
        }];
        let obj = obj.as_object().unwrap();
        let read = |name: &str| {
            p.location
                .columns
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .read(obj, &ids)
        };
        assert_eq!(read("pcode").as_deref(), Some("NG"));
        assert_eq!(read("settlement_type").as_deref(), Some("urban"));
        assert_eq!(read("ownership").as_deref(), Some("public"));
        assert_eq!(read("gers_id"), None);
    }

    #[test]
    fn a_snapshot_copy_is_used_when_no_profile_is_given() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Profile::for_snapshot(None, dir.path()).unwrap().name,
            "base"
        );
        icr_example().save_copy(dir.path());
        assert_eq!(Profile::for_snapshot(None, dir.path()).unwrap().name, "icr");
    }

    #[test]
    fn http_urls_are_refused() {
        let err = Profile::load(Some("http://example.org/p.toml")).unwrap_err();
        assert!(err.to_string().contains("https only"));
    }
}
