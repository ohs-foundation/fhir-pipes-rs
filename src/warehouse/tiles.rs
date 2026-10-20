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

use anyhow::{Context, Result, bail};

use super::config::Settings;
use super::sql::sql_path;
use super::tools;

const POINT_COLS: &str = "id, name, type, status, admin1_name, admin2_name, admin3_name, pcode, gers_id, nhfr_code, nhfr_uid,
    replace(facility_level_text, chr(160), ' ') AS facility_level, ownership_text AS ownership, settlement_type,
    managing_organization, part_of, last_updated,
    round(position_longitude, 5) AS lon, round(position_latitude, 5) AS lat";

/// Build the archives from the Location table at `locations` (the one this
/// run wrote, else the live one) into `out`, the run's staged tiles/, which
/// replaces the live one when the run is published.
pub fn build(s: &Settings, locations: &Path, out: &Path) -> Result<()> {
    let countries = countries(locations, &s.tile_countries)?;
    println!("== tiles ({})", countries.join(", "));
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
             COPY (SELECT id, name, admin_level, admin1_name, admin2_name, path, pcode, geometry
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
             COPY (SELECT {POINT_COLS}, geometry
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
