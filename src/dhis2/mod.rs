//! `fhir-pipes dhis2`: read a DHIS2 instance into FHIR NDJSON for `fhir-pipes load`.
//!
//! - `locations`: org units become ICR Locations (facilities also get their
//!   Organization), each carrying the org unit UID as an identifier under a
//!   system named after the instance.
//! - `measures`: indicators become Measures.
//! - `reports`: indicator values from `/api/analytics` become one
//!   MeasureReport per indicator, org unit and period. The org unit is mapped
//!   to its Location through that identifier.
//!
//! Every id is deterministic (`INSTANCE-<uid>…`), so re-running replaces the
//! same resources. Nothing is written to DHIS2.

pub mod api;
pub mod calendar;
pub mod expression;
pub mod locations;
pub mod measures;
pub mod raw;
pub mod reports;
pub mod rerun;

use serde_json::{Value, json};

use crate::cli::Dhis2Command;
use crate::error::{PipesError, Result};
use crate::profile::{Dhis2Profile, Profile};

/// Instance names go into every id, so they are kept short.
pub const MAX_INSTANCE_LEN: usize = 16;

/// Every system, profile and extension URL comes from the profile's [dhis2]
/// section (and the Location roles from [location] and [organization]).
pub fn run(cmd: &Dhis2Command, profile: Option<&str>) -> anyhow::Result<()> {
    let profile = Profile::load(profile)?;
    eprintln!("profile: {} ({})", profile.name, profile.source);
    profile.dhis2()?;
    match cmd {
        Dhis2Command::Locations(args) => locations::run_locations(args, &profile)?,
        Dhis2Command::Measures(args) => measures::run_measures(args, &profile)?,
        Dhis2Command::Reports(args) => reports::run_reports(args, &profile)?,
    }
    Ok(())
}

/// True for a name made of ASCII letters, digits and hyphens, 1–`max` long.
pub fn valid_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The short name an instance is filed under, and everything derived from
/// it: identifier systems, ids, canonical URLs and the meta tag, all under
/// the profile's [dhis2] base.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub urls: Dhis2Profile,
    /// [location] admin_unit_type: the type code of an org unit above facility level.
    pub admin_unit_type: Option<String>,
    /// [organization] facility_type: the type code of a facility.
    pub facility_type: Option<String>,
    /// [organization] id_prefix of a facility's Organization (`org-`).
    pub org_id_prefix: String,
    /// The boundary extension a written Location carries.
    pub boundary_extension: String,
}

impl Instance {
    pub fn new(name: &str, profile: &Profile) -> Result<Self> {
        if !valid_name(name, MAX_INSTANCE_LEN) {
            return Err(PipesError::Usage(format!(
                "--instance must be letters, digits and hyphens, at most {MAX_INSTANCE_LEN} characters, got {name:?}"
            )));
        }
        Ok(Self {
            name: name.to_string(),
            urls: profile.dhis2()?.clone(),
            admin_unit_type: profile.location.admin_unit_type.clone(),
            facility_type: profile.organization.facility_type.clone(),
            org_id_prefix: profile.organization.id_prefix.clone(),
            boundary_extension: profile.boundary_extension().to_string(),
        })
    }

    fn base(&self) -> &str {
        self.urls.base.trim_end_matches('/')
    }

    pub fn orgunit_system(&self) -> String {
        format!("{}/identifiers/dhis2/{}/orgunit", self.base(), self.name)
    }

    pub fn orgunit_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/orgunit-code",
            self.base(),
            self.name
        )
    }

    pub fn indicator_system(&self) -> String {
        format!("{}/identifiers/dhis2/{}/indicator", self.base(), self.name)
    }

    pub fn indicator_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/indicator-code",
            self.base(),
            self.name
        )
    }

    pub fn data_element_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/data-element",
            self.base(),
            self.name
        )
    }

    pub fn data_element_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/data-element-code",
            self.base(),
            self.name
        )
    }

    pub fn data_set_system(&self) -> String {
        format!("{}/identifiers/dhis2/{}/data-set", self.base(), self.name)
    }

    pub fn data_set_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/data-set-code",
            self.base(),
            self.name
        )
    }

    /// Code system of a group: `indicator-group`, `data-element-group`, or
    /// `data-element-group-set/<uid>` for the groups of one group set.
    pub fn group_system(&self, kind: &str) -> String {
        format!("{}/CodeSystem/dhis2/{}/{kind}", self.base(), self.name)
    }

    /// Code system of a breakdown's options: `dimension/<uid>` (a category,
    /// org unit group set or data element group set).
    pub fn dimension_system(&self, dimension: Option<&str>) -> String {
        match dimension {
            Some(d) => format!(
                "{}/CodeSystem/dhis2/{}/dimension/{d}",
                self.base(),
                self.name
            ),
            None => format!("{}/CodeSystem/dhis2/{}/dimension", self.base(), self.name),
        }
    }

    pub fn measure_id(&self, indicator: &str) -> String {
        format!("{}-{indicator}", self.name)
    }

    pub fn measure_url(&self, indicator: &str) -> String {
        format!("{}/Measure/{}", self.base(), self.measure_id(indicator))
    }

    pub fn tag(&self) -> Value {
        json!({"system": self.urls.project_tag_system, "code": self.name})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_short_and_plain() {
        assert!(Instance::new("sl-demo", &crate::profile::icr_example()).is_ok());
        assert!(Instance::new("", &crate::profile::icr_example()).is_err());
        assert!(Instance::new("sl-demo", &crate::profile::Profile::base()).is_err());
        assert!(Instance::new("sl demo", &crate::profile::icr_example()).is_err());
        assert!(Instance::new("sl_demo", &crate::profile::icr_example()).is_err());
        assert!(
            Instance::new("a-very-long-instance-name", &crate::profile::icr_example()).is_err()
        );
    }

    #[test]
    fn systems_and_ids_carry_the_instance() {
        let i = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        assert_eq!(
            i.orgunit_system(),
            "https://icr.healthcampaigns.org/identifiers/dhis2/sl-demo/orgunit"
        );
        assert_eq!(i.measure_id("Uvn6LCg7dVU"), "sl-demo-Uvn6LCg7dVU");
        assert_eq!(
            i.measure_url("Uvn6LCg7dVU"),
            "https://icr.healthcampaigns.org/Measure/sl-demo-Uvn6LCg7dVU"
        );
        assert_eq!(i.tag()["code"], "sl-demo");
    }
}
