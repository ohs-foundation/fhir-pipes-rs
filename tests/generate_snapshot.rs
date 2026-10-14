//! Generates a synthetic snapshot for timing and memory checks. Run with:
//!   cargo test --release --test generate_snapshot -- --ignored --nocapture
//! It writes target/bench/snapshot/locations.ndjson: 1 country, 20 states,
//! 400 districts (each a 200-vertex polygon), and N facilities (points).

use std::io::Write;
use std::path::Path;

fn polygon(cx: f64, cy: f64, r: f64, n: usize) -> String {
    let ring: Vec<String> = (0..=n)
        .map(|i| {
            let t = i as f64 / n as f64 * std::f64::consts::TAU;
            format!("[{:.6},{:.6}]", cx + r * t.cos(), cy + r * t.sin())
        })
        .collect();
    format!(
        r#"{{"type":"Polygon","coordinates":[[{}]]}}"#,
        ring.join(",")
    )
}

fn location(
    id: &str,
    name: &str,
    ty: &str,
    parent: Option<&str>,
    pcode: Option<&str>,
    position: Option<(f64, f64)>,
    boundary: Option<&str>,
) -> String {
    let mut r = serde_json::json!({"resourceType":"Location","id":id,"name":name,"status":"active",
        "meta":{"versionId":"1","lastUpdated":"2026-01-01T00:00:00Z"},
        "type":[{"coding":[{"code":ty}]}]});
    if let Some(p) = parent {
        r["partOf"] = serde_json::json!({"reference": format!("Location/{p}")});
    }
    if let Some(c) = pcode {
        r["identifier"] = serde_json::json!([{"system":"https://icr.healthcampaigns.org/identifiers/pcode","value":c}]);
    }
    if let Some((x, y)) = position {
        r["position"] = serde_json::json!({"longitude":x,"latitude":y});
    }
    if let Some(b) = boundary {
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
        r["extension"] = serde_json::json!([{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
            "valueAttachment":{"contentType":"application/geo+json","data":data}}]);
    }
    r.to_string()
}

#[test]
#[ignore]
fn generate() {
    let facilities: usize = match std::env::var("FHIR_PIPES_BENCH_FACILITIES") {
        Ok(v) => v
            .parse()
            .expect("FHIR_PIPES_BENCH_FACILITIES must be a number"),
        Err(_) => 200_000,
    };
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
    let dir = target_dir.join("bench/snapshot");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f =
        std::io::BufWriter::new(std::fs::File::create(dir.join("locations.ndjson")).unwrap());
    writeln!(
        f,
        "{}",
        location(
            "ng",
            "Nigeria",
            "admin-unit",
            None,
            Some("NG"),
            None,
            Some(&polygon(8.0, 9.0, 6.0, 400))
        )
    )
    .unwrap();
    for s in 0..20 {
        let sid = format!("s{s}");
        let (sx, sy) = (3.0 + (s % 5) as f64 * 2.5, 5.0 + (s / 5) as f64 * 2.5);
        writeln!(
            f,
            "{}",
            location(
                &sid,
                &format!("State {s}"),
                "admin-unit",
                Some("ng"),
                Some(&format!("NG{s:03}")),
                None,
                Some(&polygon(sx, sy, 1.2, 300))
            )
        )
        .unwrap();
        for d in 0..20 {
            let did = format!("{sid}d{d}");
            let (dx, dy) = (
                sx - 1.0 + (d % 5) as f64 * 0.5,
                sy - 1.0 + (d / 5) as f64 * 0.5,
            );
            writeln!(
                f,
                "{}",
                location(
                    &did,
                    &format!("District {s}-{d}"),
                    "admin-unit",
                    Some(&sid),
                    Some(&format!("NG{s:03}{d:03}")),
                    None,
                    Some(&polygon(dx, dy, 0.24, 200))
                )
            )
            .unwrap();
        }
    }
    for i in 0..facilities {
        let s = i % 20;
        let d = (i / 20) % 20;
        let (sx, sy) = (3.0 + (s % 5) as f64 * 2.5, 5.0 + (s / 5) as f64 * 2.5);
        let (dx, dy) = (
            sx - 1.0 + (d % 5) as f64 * 0.5,
            sy - 1.0 + (d / 5) as f64 * 0.5,
        );
        let jitter = (i as f64 * 0.618).fract() * 0.2 - 0.1;
        writeln!(
            f,
            "{}",
            location(
                &format!("f{i}"),
                &format!("Facility {i}"),
                "facility",
                Some(&format!("s{s}d{d}")),
                None,
                Some((dx + jitter, dy - jitter)),
                None
            )
        )
        .unwrap();
    }
    eprintln!("wrote {}", dir.join("locations.ndjson").display());
}
