//! `views validate` and `views lint`: findings about a view, printed as
//! text, JSON or SARIF.

use anyhow::Result;
use fhir_pipes_sof::ViewDefinition;
use fhir_pipes_sof_lint::{Finding, Severity};

use super::diagnostic;
use crate::error::usage;

/// Findings from checking `view`'s selectors and generated SQL against a
/// FHIR package (FH01–FH10). The package is read as every package is
/// (`fhir::package`); a name with `version` is `name#version` in the local
/// FHIR package cache, or downloaded from the FHIR package registry.
#[cfg(feature = "fhir-packages")]
pub(super) async fn package_findings(
    view: &ViewDefinition,
    package: &str,
    version: Option<&str>,
) -> Result<Vec<Finding>> {
    let spec = match version {
        Some(v) => crate::fhir::package::by_version(package, v),
        None => package.to_string(),
    };
    // Reading a package may download it, with a blocking client.
    let mut index = tokio::task::block_in_place(|| {
        crate::fhir::package::Index::load(
            std::slice::from_ref(&spec),
            "--package",
            &["StructureDefinition"],
        )
    })?;
    let provider = fhir_pipes_sof_lint::FhirSchemaProvider::from_structure_definitions(
        index
            .take_type("StructureDefinition")
            .into_values()
            .flatten(),
    );
    if provider.is_empty() {
        return Err(usage(anyhow::anyhow!(
            "--package {spec} has no StructureDefinitions of resources"
        )));
    }
    Ok(fhir_pipes_sof_lint::lint(view, &provider))
}

#[cfg(not(feature = "fhir-packages"))]
pub(super) async fn package_findings(
    _view: &ViewDefinition,
    _package: &str,
    _version: Option<&str>,
) -> Result<Vec<Finding>> {
    Err(usage(anyhow::anyhow!(
        "--package needs FHIR package support, which this fhir-pipes was built without; \
         build it with `--features fhir-packages` (--shareable works without it)"
    )))
}

/// How validate and lint report findings.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum ReportFormat {
    Human,
    Json,
    Sarif,
}

impl ReportFormat {
    pub(super) fn from_flags(json: bool, sarif: bool) -> Self {
        if sarif {
            Self::Sarif
        } else if json {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// Print findings in the chosen format and return whether any is an error.
/// JSON and SARIF print a valid empty report when there are no findings.
pub(super) fn report_findings(
    origin: &str,
    source: &str,
    findings: &[Finding],
    fmt: ReportFormat,
    color: bool,
    ok_msg: &str,
) -> bool {
    match fmt {
        ReportFormat::Json => println!("{}", diagnostic::render_findings_json(findings)),
        ReportFormat::Sarif => {
            println!("{}", diagnostic::render_findings_sarif(origin, findings))
        }
        ReportFormat::Human => {
            if findings.is_empty() {
                println!("{}", diagnostic::ok(ok_msg, color));
            } else {
                print!(
                    "{}",
                    diagnostic::render_findings(origin, source, findings, color)
                );
            }
        }
    }
    findings.iter().any(|f| f.severity == Severity::Error)
}
