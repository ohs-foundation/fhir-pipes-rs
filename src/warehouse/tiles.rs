//! Location registry → PMTiles for MapLibre maps, with duckdb (GeoParquet →
//! GeoJSON) and tippecanoe.
//!
//! - admin.pmtiles: polygons `country` (admin_level 0), `states` (1), `lgas` (2),
//!   each with id, name, admin_level, admin1_name, admin2_name, path, pcode.
//! - facilities.pmtiles, settlements.pmtiles: every point, never thinned
//!   (--drop-rate=1 --no-feature-limit --no-tile-size-limit).
//! - catchments.pmtiles: `facility_catchments`, `settlement_catchments`, where
//!   the registry has catchment areas; each carries `catchment_of` and `part_of`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

use crate::config::Settings;
use crate::tools::{self, sql_str};

const POINT_COLS: &str = "id, name, type, status, admin1_name, admin2_name, admin3_name, pcode, gers_id, nhfr_code, nhfr_uid,
    replace(facility_level_text, chr(160), ' ') AS facility_level, ownership_text AS ownership, settlement_type,
    managing_organization, part_of, last_updated,
    round(position_longitude, 5) AS lon, round(position_latitude, 5) AS lat";

pub fn build(s: &Settings) -> Result<()> {
    let locations = s.parquet_dir().join("locations");
    let countries = countries(&locations, &s.tile_countries)?;
    println!("== tiles ({})", countries.join(", "));
    let out = s.data.join("tiles");
    fs::create_dir_all(&out)?;
    let tmp = tempfile::tempdir()?;
    let t = tmp.path();
    // Each country's `dir`, where it exists, with `pattern` appended: duckdb
    // fails on a glob that matches nothing.
    let globs = |dir: &str, pattern: &str| -> Vec<PathBuf> {
        countries
            .iter()
            .map(|c| locations.join(format!("country={c}")).join(dir))
            .filter(|d| d.is_dir())
            .map(|d| d.join(pattern))
            .collect()
    };
    let list = |paths: &[PathBuf]| {
        format!(
            "[{}]",
            paths
                .iter()
                .map(|p| sql_str(p))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let poly = globs("geom_type=polygon/type=admin-unit", "*.parquet");
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
             COPY (SELECT id, name, admin_level, admin1_name, admin2_name, path, pcode, geometry
                   FROM read_parquet({poly}, union_by_name=true) WHERE admin_level = {level})
             TO {} WITH (FORMAT GDAL, DRIVER 'GeoJSON');",
                sql_str(&file)
            ),
        )?;
        println!("   {name}: {} features", features(&file));
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

    let points = globs("geom_type=point", "**/*.parquet");
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
             COPY (SELECT {POINT_COLS}, geometry
                   FROM read_parquet({points}, hive_partitioning=true, union_by_name=true)
                   WHERE type = '{kind}' AND geometry IS NOT NULL)
             TO {} WITH (FORMAT GDAL, DRIVER 'GeoJSONSeq');",
                sql_str(&file)
            ),
        )?;
        println!("   {layer}: {} features", features(&file));
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

    let catchment_globs: Vec<PathBuf> = countries
        .iter()
        .map(|c| locations.join(format!("country={c}/geom_type=polygon")))
        .filter(|d| has_catchments(d))
        .map(|d| d.join("type=*-catchment/*.parquet"))
        .collect();
    if catchment_globs.is_empty() {
        let stale = out.join("catchments.pmtiles");
        if stale.exists() {
            fs::remove_file(stale)?;
        }
        println!("   catchments: none in the registry, not built");
    } else {
        let catch = list(&catchment_globs);
        let mut layers = Vec::new();
        for kind in ["facility", "settlement"] {
            let file = t.join(format!("{kind}_catchments.geojson"));
            tools::duckdb(&s.duckdb, &format!(
                "LOAD spatial; SET geometry_always_xy = true;
                 COPY (SELECT id, name, type, status, admin1_name, admin2_name, admin3_name, path, part_of,
                              json_extract_string(fhir_json, '$.extension[*].valueReference.reference')[1] AS catchment_of,
                              last_updated, geometry
                       FROM read_parquet({catch}, hive_partitioning=true, union_by_name=true)
                       WHERE type = '{kind}-catchment' AND geometry IS NOT NULL)
                 TO {} WITH (FORMAT GDAL, DRIVER 'GeoJSONSeq');",
                sql_str(&file)
            ))?;
            println!("   {kind}_catchments: {} features", features(&file));
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
/// fails on them); with none left the archive is removed rather than left stale.
fn tippecanoe(s: &Settings, out: &Path, args: &[&str], layers: &[(&str, PathBuf)]) -> Result<()> {
    let layers: Vec<&(&str, PathBuf)> = layers.iter().filter(|(_, f)| features(f) > 0).collect();
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    if layers.is_empty() {
        if out.exists() {
            fs::remove_file(out)?;
        }
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

/// GeoJSON or GeoJSONSeq feature count, for the log.
fn features(file: &Path) -> usize {
    fs::read_to_string(file)
        .map(|t| t.matches("\"Feature\"").count())
        .unwrap_or(0)
}
