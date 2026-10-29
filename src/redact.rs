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

/// `text` with each of `sources` that is a URL replaced by [`url`] of it:
/// as given, as `url::Url` writes it, and as written without its user name
/// and password (as an HTTP client's error quotes it). Text that names a
/// source it was given is hidden exactly, whatever characters it holds.
pub fn known_urls<'a>(text: &str, sources: impl IntoIterator<Item = &'a str>) -> String {
    let mut forms: Vec<(String, String)> = Vec::new();
    for source in sources {
        let Ok(parsed) = url::Url::parse(source) else {
            continue;
        };
        let hidden = url(source);
        let mut bare = parsed.clone();
        let _ = bare.set_username("");
        let _ = bare.set_password(None);
        for form in [source.to_string(), parsed.to_string(), bare.to_string()] {
            if form != hidden && !form.is_empty() {
                forms.push((form, hidden.clone()));
            }
        }
    }
    // Longest first, so a form inside another is not replaced first.
    forms.sort_by_key(|(f, _)| std::cmp::Reverse(f.len()));
    let mut out = text.to_string();
    for (form, hidden) in forms {
        out = out.replace(&form, &hidden);
    }
    out
}

/// `text` with each URL in it (any `scheme://`, in any case) passed
/// through [`url`]: an error can quote a URL given with a token in it, such
/// as a Library source. A URL ends at whitespace, a quote, `<`, `>` or a
/// backtick; brackets and `'` can be part of it.
pub fn urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        // The scheme: letters, digits, `+`, `-` and `.`, from a letter.
        let scheme = rest[..sep]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            .last()
            .map_or(sep, |(i, _)| i);
        let start = rest[scheme..sep]
            .find(|c: char| c.is_ascii_alphabetic())
            .map_or(sep, |i| scheme + i);
        if start == sep {
            out.push_str(&rest[..sep + 3]);
            rest = &rest[sep + 3..];
            continue;
        }
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>' | '`'))
            .unwrap_or(tail.len());
        // Punctuation that ends the sentence, not the URL, and a closing
        // bracket that has no opening one in it.
        let mut found = &tail[..end];
        loop {
            let trimmed = found.trim_end_matches([':', ',', '.', ';']);
            let trimmed = match trimmed.strip_suffix(')') {
                Some(t) if t.matches('(').count() < t.matches(')').count() + 1 => t,
                _ => trimmed,
            };
            if trimmed.len() == found.len() {
                break;
            }
            found = trimmed;
        }
        out.push_str(&url(found));
        rest = &tail[found.len()..];
    }
    out.push_str(rest);
    out
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
pub(crate) const SECRET_VARS: [&str; 7] = [
    "FHIR_PIPES_TOKEN",
    "FHIR_PIPES_CLIENT_SECRET",
    "FHIR_PIPES_PASSWORD",
    "FHIR_PIPES_HAPI_DATABASE_URL",
    "FHIR_PIPES_DATABASE_URL",
    "FHIR_PIPES_SERVE_TOKEN",
    "FHIR_PIPES_QUERY_TOKEN",
];

/// The variable that names more secret variables, comma-separated: those a
/// supervisor (fhir-pipes serve) knows of from its config.
pub const SECRET_VARS_ENV: &str = "FHIR_PIPES_SECRET_VARS";

/// `text` with the value of each secret variable in the environment (4
/// characters or longer, a line at a time) replaced by `***`: SECRET_VARS
/// and those SECRET_VARS_ENV names. Applied before text that may quote a
/// secret is cut short, since a cut secret can no longer be found by whoever
/// reads it later (fhir-pipes serve).
pub fn known_secrets(text: &str) -> String {
    let named = std::env::var(SECRET_VARS_ENV).unwrap_or_default();
    let names = SECRET_VARS
        .iter()
        .copied()
        .chain(named.split(',').map(str::trim).filter(|n| !n.is_empty()));
    let mut values: Vec<String> = names
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
    fn urls_in_text_lose_their_credentials() {
        assert_eq!(
            urls(
                "libraries https://u:k1@h.org/p.tgz?token=k2: error sending request \
                 for url (https://h.org/p.tgz?token=k2)"
            ),
            "libraries https://h.org/p.tgz: error sending request for url (https://h.org/p.tgz)"
        );
        assert_eq!(urls("no url here"), "no url here");
    }

    /// URLs whose secrets sit beside brackets or a quote, or behind an
    /// uppercase scheme: (source, the secrets in it).
    const AWKWARD: [(&str, &[&str]); 5] = [
        ("https://127.0.0.1:9/p(v1).tgz?token=secret1", &["secret1"]),
        (
            "https://u:pa(ss)@host/p.tgz?token=secret2",
            &["pa(ss)", "pa%28ss%29", "secret2"],
        ),
        ("https://h.org/p.tgz?token=abc(def)", &["abc", "def"]),
        ("HTTPS://u:plain@h.org/p.tgz", &["plain"]),
        ("s3://key:it's-secret3@bucket/p.tgz", &["secret3"]),
    ];

    /// `source` quoted as errors quote it: as given, inside brackets, and
    /// as an HTTP client writes it.
    fn quoting(source: &str) -> String {
        let mut bare = url::Url::parse(source).unwrap();
        let _ = bare.set_username("");
        let _ = bare.set_password(None);
        format!("libraries {source}: error sending request for url ({bare}); '{source}'")
    }

    #[test]
    fn urls_with_brackets_quotes_or_an_uppercase_scheme_lose_their_secrets() {
        for (source, secrets) in AWKWARD {
            let text = quoting(source);
            for (how, out) in [
                ("fallback", urls(&text)),
                ("known", known_urls(&text, [source])),
                ("both", urls(&known_urls(&text, [source]))),
            ] {
                for secret in secrets {
                    assert!(!out.contains(secret), "{how}: {secret} in {out}");
                }
                assert!(out.starts_with("libraries "), "{how}: {out}");
            }
        }
        // Brackets around a URL are kept.
        assert_eq!(
            urls("for url (https://h.org/p?token=k)."),
            "for url (https://h.org/p)."
        );
        assert_eq!(urls("a:// b"), "a:// b");
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
