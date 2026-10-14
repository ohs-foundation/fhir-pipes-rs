//! The ICR `ICRTargetPopulation` profile on `Group`: a conceptual cohort
//! (`actual = false`) with a head count, scoped to a Location through the
//! `geography` characteristic, with source and date provenance in
//! extensions. Population is never a Location attribute in the ICR (the
//! georegistry rule: Location holds identity and place; revisable
//! programmatic figures live beside it), so `fhir-pipes population` emits these
//! and leaves the Location untouched. As with `location.rs`, every
//! profile-specific URL is a constant here and nowhere else.
//!
//! `estimate-date` is, per the IG, when the estimate was *made*; a modelled
//! raster only tells us its reference year, so fhir-pipes stamps `YEAR-01-01`
//! unless the operator supplies the real date.

use serde_json::{json, Value};

pub const TARGET_POPULATION_PROFILE: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/ICRTargetPopulation";
pub const GROUP_CHARACTERISTIC_SYSTEM: &str =
    "https://icr.healthcampaigns.org/CodeSystem/icr-group-characteristic-cs";
pub const DENOMINATOR_SOURCE_SYSTEM: &str =
    "https://icr.healthcampaigns.org/CodeSystem/icr-denominator-source-cs";
pub const DENOMINATOR_SOURCE_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/denominator-source";
pub const DENOMINATOR_TYPE_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/denominator-type";
pub const ESTIMATE_DATE_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/estimate-date";
pub const IS_PLANNING_DENOMINATOR_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/is-planning-denominator";
pub const IS_CALCULATED_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/is-calculated";

/// FHIR `id` is `[A-Za-z0-9\-\.]{1,64}`.
pub const MAX_ID_LEN: usize = 64;

/// R4 `unsignedInt` — `Group.quantity` cannot exceed this.
pub const MAX_QUANTITY: u64 = i32::MAX as u64;

/// FHIR `id`: `[A-Za-z0-9\-\.]{1,64}`. Location ids flow into Group ids and
/// `Location/{id}` references unchanged, so an invalid one poisons both.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Codes in `icr-denominator-source-cs` (extensible binding, so other codes are
/// allowed but worth a warning).
pub const KNOWN_SOURCE_CODES: [&str; 9] = [
    "census",
    "census-projection",
    "microcensus",
    "worldpop",
    "grid3",
    "hmis",
    "govt-estimate",
    "unknown",
    "other",
];

/// Display text for the `icr-denominator-source-cs` codes fhir-pipes emits.
pub fn source_display(code: &str) -> Option<&'static str> {
    match code {
        "worldpop" => Some("WorldPop modelled estimate"),
        "grid3" => Some("GRID3 modelled estimate"),
        _ => None,
    }
}

/// `pop-{source}-{year}-{locationId}`: deterministic, so a re-run replaces
/// the same resources.
pub fn group_id(source_code: &str, year: u16, location_id: &str) -> String {
    format!("pop-{source_code}-{year}-{location_id}")
}

pub struct TargetPopulation<'a> {
    pub location_id: &'a str,
    pub location_name: Option<&'a str>,
    pub count: u64,
    pub year: u16,
    pub source_code: &'a str,
    /// Free-text provenance: raster name and method.
    pub source_text: &'a str,
    /// ISO date for the `estimate-date` extension: when the estimate was
    /// made. The caller defaults it to `YEAR-01-01`, the raster's reference
    /// year, because the raster carries no production date; `--estimate-date`
    /// overrides.
    pub estimate_date: &'a str,
    /// Rolled up from children rather than measured directly.
    pub calculated: bool,
    pub planning_denominator: bool,
}

pub fn target_population_group(p: &TargetPopulation) -> Value {
    let scope = p.location_name.unwrap_or(p.location_id);
    let display = source_display(p.source_code);
    let mut coding = json!({"system": DENOMINATOR_SOURCE_SYSTEM, "code": p.source_code});
    if let Some(d) = display {
        coding["display"] = Value::String(d.to_string());
    }
    let group = json!({
        "resourceType": "Group",
        "id": group_id(p.source_code, p.year, p.location_id),
        "meta": {"profile": [TARGET_POPULATION_PROFILE]},
        "type": "person",
        "actual": false,
        "name": format!("Total population, {scope}, {} ({})", p.year, display.unwrap_or(p.source_code)),
        "quantity": p.count,
        "characteristic": [{
            "code": {"coding": [{"system": GROUP_CHARACTERISTIC_SYSTEM, "code": "geography", "display": "Geographic scope"}]},
            "valueReference": {"reference": format!("Location/{}", p.location_id), "display": scope},
            "exclude": false
        }],
        "extension": [
            {"url": DENOMINATOR_SOURCE_EXTENSION_URL,
             "valueCodeableConcept": {"coding": [coding], "text": p.source_text}},
            // denominator-type is a bare code in the IG (value[x] only code), unlike denominator-source.
            {"url": DENOMINATOR_TYPE_EXTENSION_URL, "valueCode": "total-population"},
            {"url": ESTIMATE_DATE_EXTENSION_URL, "valueDate": p.estimate_date},
            {"url": IS_CALCULATED_EXTENSION_URL, "valueBoolean": p.calculated},
            {"url": IS_PLANNING_DENOMINATOR_EXTENSION_URL, "valueBoolean": p.planning_denominator},
        ]
    });
    group
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(planning: bool) -> Value {
        target_population_group(&TargetPopulation {
            location_id: "lga-1",
            location_name: Some("Kano Municipal"),
            count: 512_345,
            year: 2026,
            source_code: "worldpop",
            source_text:
                "nga_pop_2026_CN_100m_cog.tif; pixel-centroid zonal sum over admin level 2",
            estimate_date: "2026-01-01",
            calculated: false,
            planning_denominator: planning,
        })
    }

    fn ext<'a>(g: &'a Value, url: &str) -> Option<&'a Value> {
        g["extension"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["url"] == url)
    }

    #[test]
    fn matches_the_icr_target_population_profile_shape() {
        let g = sample(false);
        assert_eq!(g["resourceType"], "Group");
        assert_eq!(g["id"], "pop-worldpop-2026-lga-1");
        assert_eq!(g["meta"]["profile"][0], TARGET_POPULATION_PROFILE);
        assert_eq!(g["type"], "person");
        assert_eq!(g["actual"], false);
        assert_eq!(g["quantity"], 512_345);
        assert_eq!(
            g["name"],
            "Total population, Kano Municipal, 2026 (WorldPop modelled estimate)"
        );
        let c = &g["characteristic"][0];
        assert_eq!(
            c["code"]["coding"][0]["system"],
            GROUP_CHARACTERISTIC_SYSTEM
        );
        assert_eq!(c["code"]["coding"][0]["code"], "geography");
        assert_eq!(c["valueReference"]["reference"], "Location/lga-1");
        assert_eq!(c["valueReference"]["display"], "Kano Municipal");
        assert_eq!(c["exclude"], false);
        let src = ext(&g, DENOMINATOR_SOURCE_EXTENSION_URL).unwrap();
        assert_eq!(
            src["valueCodeableConcept"]["coding"][0]["system"],
            DENOMINATOR_SOURCE_SYSTEM
        );
        assert_eq!(src["valueCodeableConcept"]["coding"][0]["code"], "worldpop");
        assert_eq!(
            src["valueCodeableConcept"]["coding"][0]["display"],
            "WorldPop modelled estimate"
        );
        assert!(src["valueCodeableConcept"]["text"]
            .as_str()
            .unwrap()
            .contains("nga_pop_2026"));
        assert_eq!(
            ext(&g, DENOMINATOR_TYPE_EXTENSION_URL).unwrap()["valueCode"],
            "total-population"
        );
        assert!(ext(&g, DENOMINATOR_TYPE_EXTENSION_URL)
            .unwrap()
            .get("valueCodeableConcept")
            .is_none());
        assert_eq!(
            ext(&g, ESTIMATE_DATE_EXTENSION_URL).unwrap()["valueDate"],
            "2026-01-01"
        );
        assert_eq!(
            ext(&g, IS_CALCULATED_EXTENSION_URL).unwrap()["valueBoolean"],
            false
        );
        assert_eq!(
            ext(&g, IS_PLANNING_DENOMINATOR_EXTENSION_URL).unwrap()["valueBoolean"],
            false
        );
    }

    #[test]
    fn planning_flag_and_display_lookup() {
        let g = sample(true);
        assert_eq!(
            ext(&g, IS_PLANNING_DENOMINATOR_EXTENSION_URL).unwrap()["valueBoolean"],
            true
        );

        let g = target_population_group(&TargetPopulation {
            location_id: "x",
            location_name: None,
            count: 1,
            year: 2027,
            source_code: "made-up",
            source_text: "t",
            estimate_date: "2027-01-01",
            calculated: true,
            planning_denominator: false,
        });
        assert_eq!(g["id"], "pop-made-up-2027-x");
        assert_eq!(g["name"], "Total population, x, 2027 (made-up)");
        let coding = &ext(&g, DENOMINATOR_SOURCE_EXTENSION_URL).unwrap()["valueCodeableConcept"]
            ["coding"][0];
        assert!(coding.get("display").is_none());
        assert_eq!(
            ext(&g, IS_CALCULATED_EXTENSION_URL).unwrap()["valueBoolean"],
            true
        );
        assert_eq!(source_display("grid3"), Some("GRID3 modelled estimate"));
    }

    #[test]
    fn the_id_is_deterministic_and_within_the_fhir_limit_for_typical_ids() {
        assert_eq!(
            group_id("worldpop", 2026, "a1b2c3d4-e5f6-7890-abcd-ef1234567890"),
            "pop-worldpop-2026-a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        );
        assert!(
            group_id("worldpop", 2026, "a1b2c3d4-e5f6-7890-abcd-ef1234567890").len() <= MAX_ID_LEN
        );
        assert_eq!(group_id("x", 2026, "l"), group_id("x", 2026, "l"));
    }

    #[test]
    fn fhir_ids_are_validated_and_dots_are_legal() {
        assert!(is_valid_id("ng") && is_valid_id("a.b-c.9") && is_valid_id(&"x".repeat(64)));
        assert!(
            !is_valid_id("")
                && !is_valid_id("a_b")
                && !is_valid_id("a:b")
                && !is_valid_id(&"x".repeat(65))
        );
        let g = target_population_group(&TargetPopulation {
            location_id: "ng.01",
            location_name: None,
            count: 7,
            year: 987,
            source_code: "worldpop",
            source_text: "t",
            estimate_date: "0987-01-01",
            calculated: false,
            planning_denominator: false,
        });
        assert_eq!(g["id"], "pop-worldpop-987-ng.01");
        assert_eq!(
            g["characteristic"][0]["valueReference"]["reference"],
            "Location/ng.01"
        );
    }
}
