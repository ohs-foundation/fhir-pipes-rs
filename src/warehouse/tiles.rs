//! Location registry → PMTiles for MapLibre maps, with duckdb (GeoParquet →
//! GeoJSON) and tippecanoe.
//!
//! - admin.pmtiles: polygons `country` (admin_level 0), `states` (1), `lgas` (2),
//!   each with id, name, admin_level, admin1_name, admin2_name, path and the
//!   profile's admin code column (ICR: pcode).
//! - facilities.pmtiles, settlements.pmtiles: every point, never thinned
//!   (--drop-rate=1 --no-feature-limit --no-tile-size-limit), with the
//!   profile's columns (`point_columns`).
//! - catchments.pmtiles: `facility_catchments`, `settlement_catchments`, where
//!   the registry has catchment areas; each carries `part_of`, and
//!   `catchment_of` when the profile names its catchment extension.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::config::Settings;
use super::sql::{sql_path, sql_str};
use super::tools;
use crate::profile::{Kind, Profile};

/// The point layers' columns, `profile`'s between the built-in ones: its
/// identifier columns (Location, then Organization); its Location type
/// columns, a type code shown as the Organization's type text under the
/// same system when the profile reads one (ICR's facility_level and
/// ownership); its Location extension codes; then any other Organization
/// columns. Type text has its non-breaking spaces made plain.
fn point_columns(profile: &Profile) -> String {
    let loc = &profile.location.columns;
    let org = &profile.organization.columns;
    let text = |name: &str| format!("replace({name}, chr(160), ' ')");
    let mut cols: Vec<String> = ["id", "name", "type", "status"]
        .into_iter()
        .chain(["admin1_name", "admin2_name", "admin3_name"])
        .map(str::to_string)
        .collect();
    let mut shown = Vec::new();
    for c in loc.iter().chain(org).filter(|c| c.kind == Kind::Identifier) {
        cols.push(c.name.clone());
    }
    for c in loc {
        match c.kind {
            Kind::TypeCoding => {
                let mirror = org
                    .iter()
                    .find(|o| o.kind == Kind::TypeText && o.system == c.system);
                cols.push(match mirror {
                    Some(o) => {
                        shown.push(o.name.as_str());
                        format!("{} AS {}", text(&o.name), c.name)
                    }
                    None => c.name.clone(),
                });
            }
            Kind::TypeText => cols.push(format!("{} AS {}", text(&c.name), c.name)),
            _ => {}
        }
    }
    for c in loc.iter().filter(|c| c.kind == Kind::ExtensionCode) {
        cols.push(c.name.clone());
    }
    for c in org {
        if c.kind != Kind::Identifier && !shown.contains(&c.name.as_str()) {
            cols.push(match c.kind {
                Kind::TypeText => format!("{} AS {}", text(&c.name), c.name),
                _ => c.name.clone(),
            });
        }
    }
    cols.extend(
        [
            "managing_organization",
            "part_of",
            "last_updated",
            "round(position_longitude, 5) AS lon",
            "round(position_latitude, 5) AS lat",
        ]
        .map(str::to_string),
    );
    cols.join(", ")
}

/// The catchment layers' `catchment_of`: the reference in the profile's
/// catchment extension.
fn catchment_of(profile: &Profile) -> Option<String> {
    let url = profile.location.catchment_extension.as_deref()?;
    Some(format!(
        "json_extract_string(list_filter(json_extract(fhir_json, '$.extension[*]'), \
         e -> json_extract_string(e, '$.url') = {})[1], '$.valueReference.reference') AS catchment_of",
        sql_str(url)
    ))
}

/// Build the archives from the Location table at `locations` (the one this
/// run wrote, else the live one) into `out`, the run's staged tiles/, which
/// replaces the live one when the run is published.
pub fn build(s: &Settings, locations: &Path, out: &Path) -> Result<()> {
    let countries = countries(locations, &s.tile_countries)?;
    println!("== tiles ({})", countries.join(", "));
    let profile = Profile::load(s.profile.as_deref())?;
    let point_cols = point_columns(&profile);
    let admin_code = profile
        .location
        .admin_code_column
        .as_deref()
        .map_or(String::new(), |c| format!(", {c}"));
    fs::create_dir_all(out)?;
    let tmp = tempfile::tempdir()?;
    let t = tmp.path();
    // The files matching `pattern` in each country's `dir`, listed for
    // duckdb rather than given as a glob: a directory name can hold `[` or
    // `{`, which duckdb would read as a pattern, and duckdb fails on a glob
    // that matches nothing.
    let globs = |dir: &str, pattern: &str| -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for c in &countries {
            let d = locations.join(format!("country={c}")).join(dir);
            if d.is_dir() {
                files.extend(matching(&d, pattern)?);
            }
        }
        Ok(files)
    };
    let list = |paths: &[PathBuf]| {
        format!(
            "[{}]",
            paths
                .iter()
                .map(|p| sql_path(p))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let poly = globs("geom_type=polygon/type=admin-unit", "*.parquet")?;
    if poly.is_empty() {
        bail!("no admin-unit polygons for {}", countries.join(", "));
    }
    let poly = list(&poly);
    for (level, name) in [(0, "country"), (1, "states"), (2, "lgas")] {
        let file = t.join(format!("{name}.geojson"));
        tools::duckdb(
            &s.duckdb,
            &format!(
                "LOAD spatial; SET geometry_always_xy = true;
             COPY (SELECT id, name, admin_level, admin1_name, admin2_name, path{admin_code}, geometry
                   FROM read_parquet({poly}, union_by_name=true) WHERE admin_level = {level})
             TO {} WITH (FORMAT GDAL, DRIVER 'GeoJSON');",
                sql_path(&file)
            ),
        )?;
        println!("   {name}: {} features", features(&file)?);
    }
    tippecanoe(
        s,
        &out.join("admin.pmtiles"),
        &[
            "--minimum-zoom=4",
            "--maximum-zoom=11",
            "--coalesce-densest-as-needed",
            "--extend-zooms-if-still-dropping",
            "--simplification=8",
            "--no-tile-size-limit",
        ],
        &[
            ("country", t.join("country.geojson")),
            ("states", t.join("states.geojson")),
            ("lgas", t.join("lgas.geojson")),
        ],
    )?;

    let points = globs("geom_type=point", "**/*.parquet")?;
    let points = if points.is_empty() {
        None
    } else {
        Some(list(&points))
    };
    for (kind, layer, zooms) in [
        (
            "facility",
            "facilities",
            ["--minimum-zoom=4", "--maximum-zoom=12", "--base-zoom=4"],
        ),
        (
            "settlement",
            "settlements",
            ["--minimum-zoom=8", "--maximum-zoom=13", "--base-zoom=8"],
        ),
    ] {
        let Some(points) = &points else {
            println!("   {layer}: no points in the registry, skipped");
            continue;
        };
        let file = t.join(format!("{layer}.geojson"));
        tools::duckdb(
            &s.duckdb,
            &format!(
                "LOAD spatial; SET geometry_always_xy = true;
             COPY (SELECT {point_cols}, geometry
                   FROM read_parquet({points}, hive_partitioning=true, union_by_name=true)
                   WHERE type = '{kind}' AND geometry IS NOT NULL)
             TO {} WITH (FORMAT GDAL, DRIVER 'GeoJSONSeq');",
                sql_path(&file)
            ),
        )?;
        println!("   {layer}: {} features", features(&file)?);
        let mut args = zooms.to_vec();
        args.extend([
            "--drop-rate=1",
            "--no-feature-limit",
            "--no-tile-size-limit",
        ]);
        tippecanoe(
            s,
            &out.join(format!("{layer}.pmtiles")),
            &args,
            &[(layer, file)],
        )?;
    }

    let mut catchment_globs: Vec<PathBuf> = Vec::new();
    for c in &countries {
        let d = locations.join(format!("country={c}/geom_type=polygon"));
        if has_catchments(&d) {
            catchment_globs.extend(matching(&d, "type=*-catchment/*.parquet")?);
        }
    }
    // `out` starts empty, so an archive not built is not in the new tiles/.
    if catchment_globs.is_empty() {
        println!("   catchments: none in the registry, not built");
    } else {
        let catch = list(&catchment_globs);
        let catchment_of = catchment_of(&profile).map_or(String::new(), |c| format!("{c},\n"));
        let mut layers = Vec::new();
        for kind in ["facility", "settlement"] {
            let file = t.join(format!("{kind}_catchments.geojson"));
            tools::duckdb(&s.duckdb, &format!(
                "LOAD spatial; SET geometry_always_xy = true;
                 COPY (SELECT id, name, type, status, admin1_name, admin2_name, admin3_name, path, part_of,
                              {catchment_of}last_updated, geometry
                       FROM read_parquet({catch}, hive_partitioning=true, union_by_name=true)
                       WHERE type = '{kind}-catchment' AND geometry IS NOT NULL)
                 TO {} WITH (FORMAT GDAL, DRIVER 'GeoJSONSeq');",
                sql_path(&file)
            ))?;
            println!("   {kind}_catchments: {} features", features(&file)?);
            layers.push((format!("{kind}_catchments"), file));
        }
        let layers: Vec<(&str, PathBuf)> = layers
            .iter()
            .map(|(n, f)| (n.as_str(), f.clone()))
            .collect();
        tippecanoe(
            s,
            &out.join("catchments.pmtiles"),
            &[
                "--minimum-zoom=6",
                "--maximum-zoom=14",
                "--coalesce-densest-as-needed",
                "--extend-zooms-if-still-dropping",
                "--simplification=4",
                "--no-tile-size-limit",
                "--no-feature-limit",
            ],
            &layers,
        )?;
    }
    Ok(())
}

/// The countries to build: those asked for, else every `country=` partition.
fn countries(locations: &Path, wanted: &[String]) -> Result<Vec<String>> {
    let present: Vec<String> = fs::read_dir(locations)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter_map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .strip_prefix("country=")
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    if wanted.is_empty() {
        let mut all = present;
        all.sort();
        if all.is_empty() {
            bail!("no country partitions in {}", locations.display());
        }
        return Ok(all);
    }
    let missing: Vec<&String> = wanted.iter().filter(|c| !present.contains(c)).collect();
    if !missing.is_empty() {
        bail!(
            "no locations for {} in {}",
            missing
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            locations.display()
        );
    }
    Ok(wanted.to_vec())
}

fn has_catchments(polygon_dir: &Path) -> bool {
    fs::read_dir(polygon_dir).is_ok_and(|it| {
        it.filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with("-catchment"))
    })
}

/// Build one archive from its layers. Empty layers are left out (tippecanoe
/// fails on them); with none left the archive is not built.
fn tippecanoe(s: &Settings, out: &Path, args: &[&str], layers: &[(&str, PathBuf)]) -> Result<()> {
    let mut kept = Vec::new();
    for layer in layers {
        if features(&layer.1)? > 0 {
            kept.push(layer);
        }
    }
    let layers = kept;
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    if layers.is_empty() {
        println!("   → {name}: no features, not built");
        return Ok(());
    }
    let mut cmd = Command::new(&s.tippecanoe);
    cmd.arg("--force")
        .arg("-o")
        .arg(out)
        .args(args)
        .arg("--quiet");
    for (layer, file) in layers {
        cmd.arg("-L").arg(format!("{layer}:{}", file.display()));
    }
    tools::run(&mut cmd)?;
    println!(
        "   → {name} ({} bytes)",
        fs::metadata(out).map(|m| m.len()).unwrap_or(0)
    );
    Ok(())
}

/// GeoJSON or GeoJSONSeq feature count. GDAL writes one feature a line, so
/// the file is read a line at a time; a file that cannot be read is an
/// error, not a layer with nothing in it.
fn features(file: &Path) -> Result<usize> {
    use std::io::BufRead as _;
    let f = fs::File::open(file).with_context(|| format!("reading {}", file.display()))?;
    let mut n = 0;
    for line in std::io::BufReader::new(f).lines() {
        let line = line.with_context(|| format!("reading {}", file.display()))?;
        n += line.matches("\"Feature\"").count();
    }
    Ok(n)
}

/// The files under `dir` that `pattern` matches, sorted. `dir` is taken as
/// it is, not as a pattern.
fn matching(dir: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let full = format!(
        "{}/{pattern}",
        glob::Pattern::escape(&dir.to_string_lossy())
    );
    let mut files = glob::glob(&full)
        .with_context(|| format!("listing {full}"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("listing {}", dir.display()))?;
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The columns the tiles had before they came from the profile.
    #[test]
    fn the_icr_profile_gives_the_icr_columns() {
        let icr = crate::profile::icr_example();
        assert_eq!(
            point_columns(&icr),
            "id, name, type, status, admin1_name, admin2_name, admin3_name, pcode, gers_id, \
             nhfr_code, nhfr_uid, replace(facility_level_text, chr(160), ' ') AS facility_level, \
             replace(ownership_text, chr(160), ' ') AS ownership, settlement_type, \
             managing_organization, part_of, last_updated, \
             round(position_longitude, 5) AS lon, round(position_latitude, 5) AS lat"
        );
        assert!(
            catchment_of(&icr)
                .unwrap()
                .contains("= 'https://icr.healthcampaigns.org/StructureDefinition/catchment-of')")
        );
    }

    #[test]
    fn without_a_profile_only_the_built_in_columns() {
        let base = Profile::base();
        assert_eq!(
            point_columns(&base),
            "id, name, type, status, admin1_name, admin2_name, admin3_name, \
             managing_organization, part_of, last_updated, \
             round(position_longitude, 5) AS lon, round(position_latitude, 5) AS lat"
        );
        assert_eq!(catchment_of(&base), None);
    }

    #[test]
    fn a_directory_name_is_not_a_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("country=[A]");
        std::fs::create_dir_all(odd.join("type=x-catchment")).unwrap();
        std::fs::write(odd.join("type=x-catchment/part-0.parquet"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("country=A")).unwrap();
        std::fs::write(dir.path().join("country=A/part-0.parquet"), "").unwrap();
        let found = matching(&odd, "type=*-catchment/*.parquet").unwrap();
        assert_eq!(found, [odd.join("type=x-catchment/part-0.parquet")]);
    }

    #[test]
    fn features_are_counted_and_an_unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("l.geojson");
        std::fs::write(
            &f,
            "{\"type\":\"FeatureCollection\",\"features\":[\n{\"type\":\"Feature\"},\n{\"type\":\"Feature\"}\n]}\n",
        )
        .unwrap();
        assert_eq!(features(&f).unwrap(), 2);
        assert!(features(&dir.path().join("missing.geojson")).is_err());
    }
}
