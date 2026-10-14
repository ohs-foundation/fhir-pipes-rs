//! Lint findings produced by the FHIR lint layer.

use std::fmt;

/// Severity of a [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A definite problem.
    Error,
    /// A likely problem worth review.
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => f.write_str("error"),
            Severity::Warning => f.write_str("warning"),
        }
    }
}

/// A single lint finding against a ViewDefinition or its generated SQL.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Stable rule code (e.g. `FH01`, or a banshee code for SQL-level findings).
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Severity.
    pub severity: Severity,
    /// Where it was found (a column name, FHIRPath selector, or SQL fragment).
    pub location: Option<String>,
    /// Link to the rule's reference page, when one exists.
    pub help_url: Option<String>,
}

impl Finding {
    /// Build a finding for an `FH*` rule, attaching the hosted rule-reference URL.
    pub fn fhir(code: &str, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            severity,
            location: None,
            help_url: Some(format!(
                "https://octofhir.github.io/sof/rules/{}",
                code.to_lowercase()
            )),
        }
    }

    /// Set the location (column name or selector).
    pub fn at(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.severity, self.code)?;
        if let Some(loc) = &self.location {
            write!(f, " ({loc})")?;
        }
        write!(f, ": {}", self.message)
    }
}
