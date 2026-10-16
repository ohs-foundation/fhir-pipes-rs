//! A small FHIRPath tokenizer for rewriting path text.
//!
//! The FHIRPath engine compiles plans only from source text, so the view
//! engine's rewrites (constant substitution, `ofType()` over choice elements)
//! still produce text. This tokenizer lets them do that without touching the
//! inside of string literals, backtick-quoted names or comments: each token
//! is a span of the source, and joining every token's text gives the source
//! back unchanged.

use std::ops::Range;

/// The kind of a [`Token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An identifier, `[A-Za-z_][A-Za-z0-9_]*`.
    Ident,
    /// A backtick-quoted identifier, quotes included.
    Quoted,
    /// A string literal in single or double quotes, quotes included.
    Str,
    /// A date, time or dateTime literal (`@2020-01-01`, `@T10:00`).
    Temporal,
    /// A number (`12`, `1.5`).
    Number,
    /// A `%` or `$` variable with its name (`%resource`, ``%`vs-x` ``, `$this`).
    Variable,
    /// A `//` or `/* */` comment.
    Comment,
    /// Whitespace.
    Space,
    /// Any other single character (`.`, `(`, `+`, ...).
    Punct,
}

/// A span of FHIRPath source text and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: Kind,
    pub span: Range<usize>,
}

/// Split `path` into tokens. Never fails: an unterminated string, quoted name
/// or comment runs to the end of the text, and the engine reports the error
/// when it parses the path.
pub(crate) fn tokens(path: &str) -> Vec<Token> {
    let bytes = path.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        let kind = match c {
            b'\'' | b'"' | b'`' => {
                i = closing(bytes, i + 1, c);
                if c == b'`' { Kind::Quoted } else { Kind::Str }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = path[i..].find(['\n', '\r']).map_or(bytes.len(), |n| i + n);
                Kind::Comment
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = path[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |n| i + 2 + n + 2);
                Kind::Comment
            }
            b'%' | b'$' => match bytes.get(i + 1) {
                Some(b'`') => {
                    i = closing(bytes, i + 2, b'`');
                    Kind::Variable
                }
                Some(&n) if is_ident_start(n) => {
                    i = ident_end(bytes, i + 1);
                    Kind::Variable
                }
                _ => {
                    i += 1;
                    Kind::Punct
                }
            },
            b'@' => {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    let fraction = b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b':') || fraction {
                        i += 1;
                    } else {
                        break;
                    }
                }
                Kind::Temporal
            }
            b'0'..=b'9' => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_digit()
                        || (bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)))
                {
                    i += 1;
                }
                Kind::Number
            }
            _ if is_ident_start(c) => {
                i = ident_end(bytes, i);
                Kind::Ident
            }
            _ if c.is_ascii_whitespace() => {
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                Kind::Space
            }
            _ => {
                // One character, which may take several bytes.
                i += path[i..].chars().next().map_or(1, char::len_utf8);
                Kind::Punct
            }
        };
        out.push(Token {
            kind,
            span: start..i,
        });
    }
    out
}

/// The end of a quoted run starting at `from` (just past the opening quote):
/// the index just past the closing `quote`, skipping backslash escapes, or the
/// end of the text.
fn closing(bytes: &[u8], from: usize, quote: u8) -> usize {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn ident_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i
}

/// The name of a [`Kind::Variable`] token's text, without its sigil or
/// backticks.
pub(crate) fn variable_name(text: &str) -> &str {
    let name = &text[1..];
    name.strip_prefix('`')
        .map(|n| n.strip_suffix('`').unwrap_or(n))
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(path: &str) -> Vec<(Kind, &str)> {
        tokens(path)
            .into_iter()
            .map(|t| (t.kind, &path[t.span]))
            .collect()
    }

    #[test]
    fn tokens_cover_the_source() {
        for path in [
            "name.where(family = '50%off').given",
            "%resource.id // 100% done\n+ %`vs-x`",
            "a /* 'x' %y */ / 2.5 + @2020-01-01T10:00:00.5+01:00",
            "'unterminated %x",
            "`a``b` \"c\\\"d\" é",
        ] {
            let joined: String = tokens(path).iter().map(|t| &path[t.span.clone()]).collect();
            assert_eq!(joined, path);
        }
    }

    #[test]
    fn literals_comments_and_quoted_names_are_single_tokens() {
        assert_eq!(
            kinds("family = '50%off'"),
            vec![
                (Kind::Ident, "family"),
                (Kind::Space, " "),
                (Kind::Punct, "="),
                (Kind::Space, " "),
                (Kind::Str, "'50%off'"),
            ]
        );
        assert_eq!(
            kinds("`a%b`.c // %d"),
            vec![
                (Kind::Quoted, "`a%b`"),
                (Kind::Punct, "."),
                (Kind::Ident, "c"),
                (Kind::Space, " "),
                (Kind::Comment, "// %d"),
            ]
        );
        assert_eq!(
            kinds("'it\\'s %x' + %y"),
            vec![
                (Kind::Str, "'it\\'s %x'"),
                (Kind::Space, " "),
                (Kind::Punct, "+"),
                (Kind::Space, " "),
                (Kind::Variable, "%y"),
            ]
        );
    }

    #[test]
    fn variables() {
        assert_eq!(
            kinds("%`vs-a b`.x+$this"),
            vec![
                (Kind::Variable, "%`vs-a b`"),
                (Kind::Punct, "."),
                (Kind::Ident, "x"),
                (Kind::Punct, "+"),
                (Kind::Variable, "$this"),
            ]
        );
        assert_eq!(variable_name("%`vs-a b`"), "vs-a b");
        assert_eq!(variable_name("%rowIndex"), "rowIndex");
        assert_eq!(kinds("% 1")[0], (Kind::Punct, "%"));
    }
}
