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
