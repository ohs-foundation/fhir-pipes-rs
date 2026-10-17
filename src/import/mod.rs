//! `fhir-pipes import`: source files a FHIR server does not have yet, as
//! Location NDJSON for `fhir-pipes load` (offline).
//!
//! - `location-boundaries`: one level of admin boundaries (GeoJSON) as the
//!   whole admin unit hierarchy above and including it.
//! - `location-sites`: rows of points (CSV: facilities, schools,
//!   settlements) as site Locations hung under that hierarchy by name, with
//!   facility Organizations if asked.
//!
//! They replace kiln's Python `bake` and `bake-points`, with the same options
//! and output. The resource shape (profiles, type system, boundary and
//! spatial index extensions) comes from the profile. A mapping problem is
//! fatal and nothing is written; a data problem in one feature or row is
//! reported and costs only that feature or row.

/// Visitor methods for the JSON types a streaming reader does not want: the
/// value is consumed and the default returned. Arrays and objects are left
/// to each visitor.
macro_rules! other_json_types_are_default {
    () => {
        fn visit_bool<E>(self, _: bool) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_i64<E>(self, _: i64) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_u64<E>(self, _: u64) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_f64<E>(self, _: f64) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_str<E>(self, _: &str) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
    };
}

pub mod location_boundaries;
pub mod location_sites;

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::error::{PipesError, Result};
use crate::fhir::location::{
    LocationKey, NewLocation, PhysicalType, boundary_extension, identifier,
};
use crate::fhir::organization::{NewOrganization, OrganizationKey};
use crate::profile::Profile;

/// The key order of the Locations import writes: kiln's, so the output is
/// byte for byte what kiln wrote.
const LOCATION_KEYS: [LocationKey; 11] = [
    LocationKey::Meta,
    LocationKey::Name,
    LocationKey::Status,
    LocationKey::Type,
    LocationKey::PhysicalType,
    LocationKey::Identifier,
    LocationKey::PartOf,
    LocationKey::Alias,
    LocationKey::Position,
    LocationKey::ManagingOrganization,
    LocationKey::Extension,
];

/// The key order of the facility Organizations import (and a `diff`
/// create) writes: kiln's.
pub(crate) const ORGANIZATION_KEYS: [OrganizationKey; 5] = [
    OrganizationKey::Meta,
    OrganizationKey::Active,
    OrganizationKey::Name,
    OrganizationKey::Type,
    OrganizationKey::Identifier,
];

#[derive(clap::Subcommand, Debug)]
pub enum ImportCommand {
    /// One level of admin boundaries (a GeoJSON FeatureCollection) as the admin unit
    /// hierarchy: the country, every level named by --level, the last one with its boundary
    LocationBoundaries(LocationBoundariesArgs),
    /// Rows of points (CSV: facilities, schools, settlements) as site Locations under the
    /// admin units location-boundaries wrote, optionally each with a facility Organization
    LocationSites(Box<LocationSitesArgs>),
}

#[derive(clap::Args, Debug, Clone)]
pub struct LocationBoundariesArgs {
    /// GeoJSON FeatureCollection, one feature per unit of the last --level, in WGS84
    #[arg(long = "in")]
    pub input: PathBuf,
    /// The country, the root of the hierarchy, as NAME=CODE (e.g. Nigeria=NGA); its id is the code as a slug
    #[arg(long)]
    pub country: String,
    /// LEVEL=NAME_PROP[:CODE_PROP], repeatable and ordered top-down (e.g. state=state:statecode lga=lga ward=ward). The last level is the feature level and carries the boundary. A unit's id is its parent's id, a hyphen, and the slug of its code (or name)
    #[arg(long = "level", required = true)]
    pub levels: Vec<String>,
    /// LEVEL=PROPERTY holding ';'-separated alternate names, written as alias; repeatable
    #[arg(long = "alias")]
    pub aliases: Vec<String>,
    /// Identifier system of the admin codes; defaults to the profile's [location] admin_code_system (or [dhis2] country_code_system)
    #[arg(long)]
    pub code_system: Option<String>,
    /// Also give the country and every level above the last a boundary: the union of its children's (consistent with them, not authoritative)
    #[arg(long)]
    pub dissolve_parents: bool,
    /// Output NDJSON file of Location resources, parents first
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct LocationSitesArgs {
    /// CSV file with a header row
    #[arg(long = "in")]
    pub input: PathBuf,
    /// NDJSON of the admin units to hang sites under: the output of `import location-boundaries`
    #[arg(long)]
    pub boundaries: PathBuf,
    /// Location type code of every row (e.g. facility, school, settlement), under the profile's type system
    #[arg(long = "type", value_name = "CODE")]
    pub type_code: String,
    /// Column holding the site's name
    #[arg(long)]
    pub name_col: String,
    /// Column holding the latitude (WGS84)
    #[arg(long)]
    pub lat_col: String,
    /// Column holding the longitude (WGS84)
    #[arg(long)]
    pub lon_col: String,
    /// Column holding a stable unique id, used as the Location id
    #[arg(long)]
    pub id_col: String,
    /// LEVEL=COLUMN, repeatable and ordered top-down (e.g. state=state lga=lga ward=ward): the admin unit names of the row, matched by name or alias. A row links to the deepest level that matches
    #[arg(long = "parent", required = true)]
    pub parents: Vec<String>,
    /// SYSTEM_URI=COLUMN, repeatable: an identifier on the Location; empty values are left out
    #[arg(long = "identifier")]
    pub identifiers: Vec<String>,
    /// COLUMN=VALUE, repeatable: only rows where every one matches exactly
    #[arg(long = "where")]
    pub wheres: Vec<String>,
    /// Also write a facility Organization per row (id <[organization] id_prefix><row id>), named by the Location's managingOrganization
    #[arg(long)]
    pub paired_org: bool,
    /// SYSTEM_URI=COLUMN, repeatable: an identifier on the paired Organization (registry codes)
    #[arg(long = "org-identifier", requires = "paired_org")]
    pub org_identifiers: Vec<String>,
    /// SYSTEM_URI=CODE_COLUMN[:TEXT_COLUMN], repeatable: a type on the paired Organization after `prov`. The code is the cell as a slug, its display the cell, the concept text the text column
    #[arg(long = "org-type-coding", requires = "paired_org")]
    pub org_type_codings: Vec<String>,
    /// SYSTEM_URI=CODE_COLUMN[:TEXT_COLUMN], repeatable: as --org-type-coding, on the Location's type after its type code
    #[arg(long = "type-coding")]
    pub type_codings: Vec<String>,
    /// SCHEME:LEVEL, repeatable (e.g. quadkey:18, geohash:8): the profile's spatial index extension on every row with a position
    #[arg(long = "spatial-index")]
    pub spatial_indexes: Vec<String>,
    /// Output NDJSON file of Location (and Organization) resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

pub fn run(cmd: &ImportCommand, profile: Option<&str>) -> Result<()> {
    let profile = Profile::load(profile)?;
    eprintln!("profile: {} ({})", profile.name, profile.source);
    match cmd {
        ImportCommand::LocationBoundaries(args) => location_boundaries::run(args, &profile),
        ImportCommand::LocationSites(args) => location_sites::run(args, &profile),
    }
}

/// Lower case, accents folded to ASCII (NFKD), each run of anything but a
/// letter or digit made one hyphen, none at either end: `N'Djaména` →
/// `n-djamena`. The same as kiln's Python slugify, so ids do not change.
pub fn slugify(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.nfkd().filter(char::is_ascii) {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

/// A property or cell as text the way the Python read it (`str(value or
/// "")`): missing, null, false, 0 and empty are all "".
pub fn text_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => String::new(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Array(a)) if a.is_empty() => String::new(),
        Some(Value::Object(o)) if o.is_empty() => String::new(),
        Some(other) => other.to_string(),
    }
}

/// `LEFT=RIGHT` split at the first `=`, both sides non-empty.
fn split_pair<'a>(arg: &'a str, flag: &str, form: &str) -> Result<(&'a str, &'a str)> {
    match arg.split_once('=') {
        Some((l, r)) if !l.is_empty() && !r.is_empty() => Ok((l, r)),
        _ => Err(PipesError::Usage(format!(
            "{flag} must be {form}, got {arg:?}"
        ))),
    }
}

/// One CodeableConcept added to a type: (system, code, display, text).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeConcept {
    pub system: String,
    pub code: String,
    pub display: String,
    pub text: Option<String>,
}

impl TypeConcept {
    fn json(&self) -> Value {
        let mut concept = json!({"coding": [{"system": self.system, "code": self.code, "display": self.display}]});
        if let Some(text) = &self.text {
            concept["text"] = json!(text);
        }
        concept
    }
}

/// A point Location to build: everything but the fixed parts.
#[derive(Debug, Default)]
pub struct Site<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub type_code: &'a str,
    pub parent: Option<&'a str>,
    pub identifiers: Vec<(String, String)>,
    /// (longitude, latitude).
    pub position: Option<(f64, f64)>,
    pub managing_organization: Option<&'a str>,
    pub extra_types: Vec<TypeConcept>,
    /// Spatial index extensions, already built.
    pub extensions: Vec<Value>,
}

/// What the profile says the written resources look like.
#[derive(Debug, Clone)]
pub struct Shape {
    pub location_profile: Option<String>,
    pub organization_profile: Option<String>,
    pub type_system: String,
    pub admin_unit_type: Option<String>,
    pub boundary_extension: String,
}

impl Shape {
    /// `command` is for the message when the profile has no type system.
    pub fn new(profile: &Profile, command: &str) -> Result<Shape> {
        let type_system = profile.location_type_system().ok_or_else(|| {
            PipesError::Usage(format!(
                "import {command} needs the system of the Location type codes: \
                 [location] type_systems in the profile; {} has none. Pass --profile; \
                 see docs/profiles.md",
                profile.source
            ))
        })?;
        Ok(Shape {
            location_profile: profile.location_profile().map(str::to_string),
            organization_profile: profile.organization_profile().map(str::to_string),
            type_system: type_system.to_string(),
            admin_unit_type: profile.location.admin_unit_type.clone(),
            boundary_extension: profile.boundary_extension().to_string(),
        })
    }

    fn meta(profile: Option<&str>) -> Option<Value> {
        profile.map(|p| json!({"profile": [p]}))
    }

    fn identifiers(ids: &[(String, String)]) -> Vec<Value> {
        ids.iter()
            .map(|(system, value)| identifier(system, value))
            .collect()
    }

    /// An admin unit: a jurisdiction of `[location] admin_unit_type`, with at
    /// least one identifier, and its boundary when it has one.
    pub fn admin_unit(
        &self,
        id: &str,
        name: &str,
        parent: Option<&str>,
        identifiers: &[(String, String)],
        aliases: &[String],
        boundary: Option<&[u8]>,
    ) -> Value {
        debug_assert!(!identifiers.is_empty(), "admin unit {id} has no identifier");
        NewLocation {
            id,
            meta: Self::meta(self.location_profile.as_deref()),
            extension: boundary
                .map(|geojson| boundary_extension(&self.boundary_extension, geojson))
                .into_iter()
                .collect(),
            identifier: Self::identifiers(identifiers),
            status: "active",
            name,
            alias: aliases,
            type_system: &self.type_system,
            type_code: self.admin_unit_type.as_deref().unwrap_or_default(),
            physical_type: Some(PhysicalType::Jurisdiction),
            part_of: parent,
            ..Default::default()
        }
        .into_json(&LOCATION_KEYS)
    }

    /// Add the boundary extension to a Location.
    pub fn attach_boundary(&self, location: &mut Value, geojson: &[u8]) {
        let Some(obj) = location.as_object_mut() else {
            return;
        };
        let ext = boundary_extension(&self.boundary_extension, geojson);
        match obj.get_mut("extension").and_then(Value::as_array_mut) {
            Some(list) => list.push(ext),
            None => {
                obj.insert("extension".into(), json!([ext]));
            }
        }
    }

    /// A site: `si` physical type, a position rather than a boundary.
    pub fn site(&self, s: &Site) -> Value {
        NewLocation {
            id: s.id,
            meta: Self::meta(self.location_profile.as_deref()),
            extension: s.extensions.clone(),
            identifier: Self::identifiers(&s.identifiers),
            status: "active",
            name: s.name,
            type_system: &self.type_system,
            type_code: s.type_code,
            extra_types: s.extra_types.iter().map(TypeConcept::json).collect(),
            physical_type: Some(PhysicalType::Site),
            position: s.position,
            managing_organization: s.managing_organization,
            part_of: s.parent,
            ..Default::default()
        }
        .into_json(&LOCATION_KEYS)
    }

    /// The facility Organization paired with a site: `prov` first, then
    /// `types`; registry codes as its identifiers.
    pub fn facility_organization(
        &self,
        id: &str,
        name: &str,
        identifiers: &[(String, String)],
        types: &[TypeConcept],
    ) -> Value {
        NewOrganization {
            id,
            meta: Self::meta(self.organization_profile.as_deref()),
            identifier: Self::identifiers(identifiers),
            active: true,
            extra_types: types.iter().map(TypeConcept::json).collect(),
            name: Some(name),
        }
        .into_json(&ORGANIZATION_KEYS)
    }
}

#[cfg(test)]
pub(crate) fn icr_shape() -> Shape {
    Shape::new(&crate::profile::icr_example(), "test").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fhir::location::PHYSICAL_TYPE_SYSTEM;
    use base64::Engine;

    const ICR_LOCATION: &str = "https://icr.healthcampaigns.org/StructureDefinition/ICRLocation";
    const NATIONAL_ADMIN_CODE: &str =
        "https://icr.healthcampaigns.org/identifiers/national-admin-code";

    #[test]
    fn slugify_lowercases_folds_and_hyphenates() {
        assert_eq!(slugify("Alkaleri East"), "alkaleri-east");
        assert_eq!(slugify("Grand-Popo / Centre"), "grand-popo-centre");
        assert_eq!(slugify("  N'Djaména "), "n-djamena");
        assert_eq!(slugify("A  --  B"), "a-b");
        assert_eq!(slugify("!!"), "");
    }

    #[test]
    fn text_of_reads_like_python_str_or_empty() {
        assert_eq!(text_of(None), "");
        assert_eq!(text_of(Some(&json!(0))), "");
        assert_eq!(text_of(Some(&json!(12))), "12");
        assert_eq!(text_of(Some(&json!(1.5))), "1.5");
        assert_eq!(text_of(Some(&json!(" x "))), " x ");
    }

    #[test]
    fn an_admin_unit_has_the_icr_shape() {
        let shape = icr_shape();
        let ids = [(NATIONAL_ADMIN_CODE.to_string(), "BA".to_string())];
        let r = shape.admin_unit("nga-ba", "Bauchi", Some("nga"), &ids, &[], None);
        assert_eq!(r["resourceType"], "Location");
        assert_eq!(r["meta"]["profile"], json!([ICR_LOCATION]));
        assert_eq!(r["status"], "active");
        assert_eq!(
            r["type"][0]["coding"][0],
            json!({"system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs", "code": "admin-unit"})
        );
        assert_eq!(
            r["physicalType"]["coding"][0]["system"],
            PHYSICAL_TYPE_SYSTEM
        );
        assert_eq!(r["physicalType"]["coding"][0]["code"], "jdn");
        assert_eq!(r["partOf"], json!({"reference": "Location/nga"}));
        assert_eq!(
            r["identifier"],
            json!([{"system": NATIONAL_ADMIN_CODE, "value": "BA"}])
        );
        assert!(r.get("alias").is_none() && r.get("extension").is_none());

        let root = shape.admin_unit("nga", "Nigeria", None, &ids, &[], None);
        assert!(root.get("partOf").is_none());
    }

    #[test]
    fn an_admin_unit_inlines_its_boundary_as_base64() {
        let geojson = br#"{"type":"Polygon","coordinates":[[[3,6],[4,6],[4,7],[3,7],[3,6]]]}"#;
        let ids = [(NATIONAL_ADMIN_CODE.to_string(), "x".to_string())];
        let r = icr_shape().admin_unit(
            "w",
            "W",
            Some("p"),
            &ids,
            &["Alkaleri E.".to_string()],
            Some(geojson),
        );
        assert_eq!(r["alias"], json!(["Alkaleri E."]));
        let ext = &r["extension"][0];
        assert_eq!(
            ext["url"],
            "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson"
        );
        assert_eq!(
            ext["valueAttachment"]["contentType"],
            "application/geo+json"
        );
        let data = base64::engine::general_purpose::STANDARD
            .decode(ext["valueAttachment"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(data, geojson);
    }

    #[test]
    fn without_profile_urls_meta_is_left_out() {
        let mut profile = Profile::parse(
            "[location]\nadmin_unit_type = \"admin-unit\"\ntype_systems = [\"https://example.org/cs/type\"]\n",
            "t.toml",
        )
        .unwrap();
        let shape = Shape::new(&profile, "test").unwrap();
        let ids = [("s".to_string(), "v".to_string())];
        let r = shape.admin_unit("x", "X", None, &ids, &[], Some(b"{}"));
        assert!(r.get("meta").is_none());
        assert_eq!(
            r["extension"][0]["url"],
            crate::fhir::location::HL7_BOUNDARY_EXTENSION_URL
        );
        profile.location.type_systems.clear();
        let err = Shape::new(&profile, "location-boundaries").unwrap_err();
        assert!(err.to_string().contains("type_systems"), "{err}");
    }
}
