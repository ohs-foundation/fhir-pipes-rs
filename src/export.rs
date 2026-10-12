//! `kiln export`: download every resource of the given types from a FHIR
//! server, one NDJSON file per type. Unlike `extract` it keeps no snapshot or
//! watermark; each run is a full download, written atomically.

use std::time::Duration;

use crate::cli::ExportArgs;
use crate::error::{KilnError, Result};
use crate::extract::client::FhirClient;
use crate::extract::page::{count_resources, page_resources};
use crate::report::Report;
use crate::transform::write_report;

/// A FHIR resource type name: an upper-case letter, then letters. It becomes
/// a file name, so nothing else is accepted.
fn valid_type(t: &str) -> bool {
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphabetic())
}

pub fn run_export(args: &ExportArgs) -> Result<()> {
    let mut types: Vec<&str> = Vec::new();
    for t in &args.types {
        if !valid_type(t) {
            return Err(KilnError::Usage(format!(
                "--type {t}: not a FHIR resource type name (e.g. MeasureReport)"
            )));
        }
        if !types.contains(&t.as_str()) {
            types.push(t);
        }
    }
    std::fs::create_dir_all(&args.out)
        .map_err(|e| KilnError::Usage(format!("--out {}: {e}", args.out.display())))?;
    let client = FhirClient::new(
        args.auth.auth(),
        args.retries,
        Duration::from_secs(args.timeout),
    )?;
    let mut report = Report::default();
    for t in types {
        export_type(&client, args, t, &mut report)?;
    }
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    println!("{}", report.summary());
    Ok(())
}

/// Page `<server>/<resource_type>` into OUT/<resource_type>.ndjson. The file
/// is written beside the target and renamed into place, so a failed run
/// leaves the previous download intact.
fn export_type(
    client: &FhirClient,
    args: &ExportArgs,
    resource_type: &str,
    report: &mut Report,
) -> Result<()> {
    let out = args.out.join(format!("{resource_type}.ndjson"));
    let tmp = args.out.join(format!(".{resource_type}.ndjson.tmp"));
    let paged = match page_resources(client, &args.server, resource_type, None, &tmp, report) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, &out).map_err(|e| KilnError::io(&out, e))?;
    let total = paged.notes.len();
    // As in extract: a last page that was full with no next link may mean
    // the server stopped paging early, so only then is the count worth a
    // request (a count over a large table is not free).
    if paged.last_page_full {
        match count_resources(client, &args.server, resource_type) {
            Some(server_total) if server_total as usize != total => report.add(
                "count_mismatch",
                resource_type,
                &format!(
                    "the server reports {server_total} {resource_type} resources but the export paged {total}; \
                     the server stopped paging early (no next link) or its count is stale"
                ),
            ),
            Some(_) => {}
            None => eprintln!(
                "count check unavailable for {resource_type}: the server returned no total"
            ),
        }
    }
    println!(
        "{resource_type}: {total} resources, {} pages -> {}",
        paged.pages,
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names_are_plain_words() {
        assert!(valid_type("MeasureReport"));
        assert!(valid_type("Location"));
        assert!(!valid_type(""));
        assert!(!valid_type("measureReport"));
        assert!(!valid_type("../Location"));
        assert!(!valid_type("Location?_count=1"));
    }
}
