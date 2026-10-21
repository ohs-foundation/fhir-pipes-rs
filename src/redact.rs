//! Text that may hold a secret, made fit to print or record.

/// `url` without a user name, password or query: a server URL can carry
/// credentials (`https://user:pass@host`) and a storage URL a signature
/// (`?X-Amz-Signature=…`). A URL with none of them, or text that is not a
/// URL, comes back as it is.
pub fn url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() || u.query().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.to_string()
        }
        _ => url.to_string(),
    }
}

/// `text` with every single-quoted string of `sql` in it blanked: a tool's
/// error can quote the SQL it ran, and that SQL can hold keys
/// (`CREATE SECRET … KEY_ID '…'`).
pub fn sql_strings(text: &str, sql: &str) -> String {
    let mut out = text.to_string();
    let mut rest = sql;
    while let Some(start) = rest.find('\'') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('\'') else { break };
        let literal = &after[..end];
        if !literal.is_empty() {
            out = out.replace(literal, "***");
        }
        rest = &after[end + 1..];
    }
    out
}

/// The variables whose values refresh holds as secrets.
const SECRET_VARS: [&str; 6] = [
    "FHIR_PIPES_TOKEN",
    "FHIR_PIPES_CLIENT_SECRET",
    "FHIR_PIPES_PASSWORD",
    "FHIR_PIPES_HAPI_DATABASE_URL",
    "FHIR_PIPES_DATABASE_URL",
    "FHIR_PIPES_SERVE_TOKEN",
];

/// `text` with the value of each secret variable in the environment (4
/// characters or longer, a line at a time) replaced by `***`. Applied before
/// text that may quote a secret is cut short, since a cut secret can no
/// longer be found by whoever reads it later (fhir-pipes serve).
pub fn known_secrets(text: &str) -> String {
    let mut values: Vec<String> = SECRET_VARS
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .flat_map(|v| {
            v.lines()
                .map(str::trim_end)
                .filter(|l| l.len() >= 4)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    // The longest first, so a value inside another is not matched first.
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    let mut out = text.to_string();
    for v in values {
        out = out.replace(&v, "***");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_and_queries_are_dropped() {
        assert_eq!(url("https://u:p@h.org/fhir"), "https://h.org/fhir");
        assert_eq!(
            url("https://b.s3.amazonaws.com/f.ndjson?X-Amz-Signature=abc"),
            "https://b.s3.amazonaws.com/f.ndjson"
        );
        // Anything else as it was, trailing slash or not.
        assert_eq!(url("https://h.org/fhir"), "https://h.org/fhir");
        assert_eq!(url("Binary/1"), "Binary/1");
    }

    #[test]
    fn quoted_sql_strings_are_blanked() {
        let sql = "CREATE SECRET (TYPE s3, KEY_ID 'AKIA1', SECRET 'shh');";
        let err = "Parser Error: near KEY_ID 'AKIA1', SECRET 'shh'";
        assert_eq!(
            sql_strings(err, sql),
            "Parser Error: near KEY_ID '***', SECRET '***'"
        );
    }
}
