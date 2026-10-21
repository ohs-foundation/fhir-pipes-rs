//! The FHIRPath operator-precedence table (N1, §6.8), run through the parser
//! fhir-pipes uses, the way fhir-pipes runs any FHIRPath (see
//! `common::evaluate`). It catches precedence bugs such as
//! octofhir/fhirpath-rs#176.
//!
//! The cases are generated from `LEVELS`. For each pair of operators at
//! different levels, in either order, and for each pair at the same level
//! (binary operators are left-associative), an expression such as
//! `a OP1 b OP2 c` is compared with the same expression written with the
//! grouping the spec gives, in parentheses. The operands are the first ones,
//! from small lists of literals per operator, for which the spec's grouping
//! gives a result and the other grouping gives a different result or an
//! error. Only the parenthesised forms are used to choose them, so the choice
//! does not depend on how the engine reads precedence. A pair for which no
//! operands are found is not run; `SKIPPED` lists those pairs with the reason,
//! and the test fails if the list is out of date.
//!
//! Failures are listed in `tests/fhirpath-precedence/known-failures.json`
//! with their cause, enforced both ways as for the HL7 suite:
//! `FHIRPATH_UPDATE_KNOWN_FAILURES=1 cargo test -p fhir-pipes-sof --test fhirpath_precedence`
//! rewrites it.

// Uses `evaluate` and the known-failures check, not `number`.
#[allow(dead_code)]
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use common::check_known_failures;
use serde_json::{Value, json};

/// How an operator is written.
#[derive(Clone, Copy)]
enum Kind {
    /// After its operand: `.count()` for invocation, `[1]` for the indexer.
    Postfix,
    /// Before its operand: unary `-`.
    Prefix,
    /// Between two expressions.
    Binary,
    /// `is` and `as`, whose right side is a type name, not an expression.
    Type,
}

/// The precedence table, from high to low. Unary `+` is left out: it gives
/// its operand unchanged, so no grouping changes a result.
const LEVELS: &[(Kind, &[&str])] = &[
    (Kind::Postfix, &[".count()"]),
    (Kind::Postfix, &["[1]"]),
    (Kind::Prefix, &["-"]),
    (Kind::Binary, &["*", "/", "div", "mod"]),
    (Kind::Binary, &["+", "-", "&"]),
    (Kind::Type, &["is", "as"]),
    (Kind::Binary, &["|"]),
    (Kind::Binary, &[">", "<", ">=", "<="]),
    (Kind::Binary, &["=", "~", "!=", "!~"]),
    (Kind::Binary, &["in", "contains"]),
    (Kind::Binary, &["and"]),
    (Kind::Binary, &["xor", "or"]),
    (Kind::Binary, &["implies"]),
];

/// The pairs no operands are found for, as `higher / lower` (or `left / right`
/// within a level), by reason.
const SKIPPED: &[(&str, &[&str])] = &[
    (
        "the operator is associative: both groupings give the same result",
        &[
            "& / &",
            "* / *",
            "+ / +",
            "| / |",
            "and / and",
            "or / or",
            "xor / xor",
        ],
    ),
    (
        "a comparison gives a Boolean, which `<`, `<=`, `>` and `>=` cannot order, \
         so neither grouping is well-typed",
        &[
            "< / <", "< / <=", "< / >", "< / >=", "<= / <", "<= / <=", "<= / >", "<= / >=",
            "> / <", "> / <=", "> / >", "> / >=", ">= / <", ">= / <=", ">= / >", ">= / >=",
        ],
    ),
    (
        "`a < b is T` orders a Boolean in either grouping",
        &["is / <", "is / <=", "is / >", "is / >="],
    ),
];

/// The literals tried as operands of an operator.
fn operands(op: &str) -> &'static [&'static str] {
    match op {
        "*" | "/" | "div" | "mod" | "-" => &["7", "3", "2", "1.5"],
        "+" => &["7", "3", "2", "'a'", "'b'"],
        "&" => &["'a'", "'b'", "{}"],
        "|" | "in" | "contains" => &["1", "2", "(1 | 2)", "'a'", "true", "false"],
        ">" | "<" | ">=" | "<=" => &["1", "2", "'a'", "'b'"],
        "=" | "~" | "!=" | "!~" => &["1", "2", "'a'", "true", "false", "{}"],
        "and" | "xor" | "or" | "implies" => &["true", "false", "{}"],
        "is" | "as" => &["Integer", "Decimal", "String", "Boolean"],
        // Operands of `.count()`, `[1]` and unary `-`.
        _ => &["7", "1.5", "(1 | 2)", "'a'", "true"],
    }
}

/// The operands of the middle slot, which belong to both operators.
fn middle(op1: &str, op2: &str) -> Vec<&'static str> {
    let mut list = operands(op1).to_vec();
    for o in operands(op2) {
        if !list.contains(o) {
            list.push(o);
        }
    }
    list
}

/// One way to write a pair: the bare expression, the spec's grouping and the
/// other grouping.
struct Form {
    bare: String,
    right: String,
    wrong: String,
}

/// The forms to try for a pair, in order. `hi` binds tighter than `lo`, or,
/// with `same`, both are at one level and `hi` comes first in the expression.
fn forms(hi: (Kind, &str), lo: (Kind, &str), same: bool) -> Option<Vec<Form>> {
    let mut list = Vec::new();
    let mut push =
        |bare: String, right: String, wrong: String| list.push(Form { bare, right, wrong });
    match (hi, lo) {
        ((Kind::Binary, x), (Kind::Binary, y)) => {
            // `a x b y c` groups as `(a x b) y c`, whether `x` binds tighter
            // or both are at one level.
            for a in operands(x) {
                for b in middle(x, y) {
                    for c in operands(y) {
                        push(
                            format!("{a} {x} {b} {y} {c}"),
                            format!("({a} {x} {b}) {y} {c}"),
                            format!("{a} {x} ({b} {y} {c})"),
                        );
                    }
                }
            }
            if !same {
                // `a y b x c` groups as `a y (b x c)`.
                for a in operands(y) {
                    for b in middle(y, x) {
                        for c in operands(x) {
                            push(
                                format!("{a} {y} {b} {x} {c}"),
                                format!("{a} {y} ({b} {x} {c})"),
                                format!("({a} {y} {b}) {x} {c}"),
                            );
                        }
                    }
                }
            }
        }
        ((Kind::Binary, x), (Kind::Type, t)) | ((Kind::Type, t), (Kind::Binary, x)) => {
            // `a x b is T`: the operator that binds tighter takes `b`.
            let x_first = matches!(hi.0, Kind::Binary);
            for a in operands(x) {
                for b in middle(x, "") {
                    for ty in operands(t) {
                        let (right, wrong) = (
                            format!("({a} {x} {b}) {t} {ty}"),
                            format!("{a} {x} ({b} {t} {ty})"),
                        );
                        let (right, wrong) = if x_first {
                            (right, wrong)
                        } else {
                            (wrong, right)
                        };
                        push(format!("{a} {x} {b} {t} {ty}"), right, wrong);
                    }
                }
            }
        }
        ((Kind::Postfix, p), (Kind::Binary, y)) => {
            // `a y b.count()` groups as `a y (b.count())`.
            for a in operands(y) {
                for b in middle(y, "") {
                    push(
                        format!("{a} {y} {b}{p}"),
                        format!("{a} {y} ({b}{p})"),
                        format!("({a} {y} {b}){p}"),
                    );
                }
            }
        }
        // Negation gives the same result on either side of `*`, `/`, `div`
        // and `mod` (both divisions truncate), so the grouping cannot be seen.
        ((Kind::Prefix, _), (Kind::Binary, "*" | "/" | "div" | "mod")) => return None,
        ((Kind::Prefix, u), (Kind::Binary, y)) => {
            // `-a y b` groups as `(-a) y b`.
            for a in operands(u) {
                for b in operands(y) {
                    push(
                        format!("{u}{a} {y} {b}"),
                        format!("({u}{a}) {y} {b}"),
                        format!("{u}({a} {y} {b})"),
                    );
                }
            }
        }
        ((Kind::Prefix, u), (Kind::Type, t)) => {
            // `-a is T` groups as `(-a) is T`.
            for a in operands(u) {
                for ty in operands(t) {
                    push(
                        format!("{u}{a} {t} {ty}"),
                        format!("({u}{a}) {t} {ty}"),
                        format!("{u}({a} {t} {ty})"),
                    );
                }
            }
        }
        ((Kind::Postfix, p), (Kind::Prefix, u)) => {
            // `-a.count()` groups as `-(a.count())`.
            for a in operands(p) {
                push(
                    format!("{u}{a}{p}"),
                    format!("{u}({a}{p})"),
                    format!("({u}{a}){p}"),
                );
            }
        }
        // Invocation and the indexer both follow their operand and apply left
        // to right, so only one grouping can be written. After `is` or `as`,
        // `.count()` would read as part of the type name. Two type operators
        // in a row have one well-formed grouping, since a type name is not an
        // expression.
        _ => return None,
    }
    Some(list)
}

/// An operator as `SKIPPED` names it.
fn name(kind: Kind, op: &str) -> String {
    match kind {
        Kind::Prefix => format!("unary {op}"),
        _ => op.to_string(),
    }
}

/// The values of an expression, or the error.
async fn eval(expression: &str) -> Result<Vec<Value>, String> {
    let input = json!({ "resourceType": "Patient", "id": "p" });
    common::evaluate(expression, None, json!([]), &input).await
}

/// The first form whose spec grouping gives a result and whose other grouping
/// gives a different one, preferring a different result to an error.
async fn choose(forms: Vec<Form>) -> Option<(Form, Vec<Value>)> {
    let mut fallback = None;
    for form in forms {
        let Ok(right) = eval(&form.right).await else {
            continue;
        };
        match eval(&form.wrong).await {
            Ok(wrong) if wrong != right => return Some((form, right)),
            Err(_) if fallback.is_none() => fallback = Some((form, right)),
            _ => {}
        }
    }
    fallback
}

#[tokio::test(flavor = "multi_thread")]
async fn fhirpath_precedence() {
    let ops: Vec<(usize, Kind, &str)> = LEVELS
        .iter()
        .enumerate()
        .flat_map(|(level, (kind, ops))| ops.iter().map(move |op| (level, *kind, *op)))
        .collect();

    let mut skipped = BTreeSet::new();
    let mut failures = BTreeMap::new();
    let mut run = 0;
    for &(l1, k1, op1) in &ops {
        for &(l2, k2, op2) in &ops {
            // Each pair at different levels once, higher first; each ordered
            // pair within a level.
            if l1 > l2 {
                continue;
            }
            let Some(forms) = forms((k1, op1), (k2, op2), l1 == l2) else {
                continue;
            };
            let Some((form, expected)) = choose(forms).await else {
                skipped.insert(format!("{} / {}", name(k1, op1), name(k2, op2)));
                continue;
            };
            run += 1;
            let got = eval(&form.bare).await;
            if got.as_ref() != Ok(&expected) {
                let got = match got {
                    Ok(values) => Value::from(values).to_string(),
                    Err(e) => e,
                };
                let reason = format!(
                    "expected {} (as {}), got {}",
                    Value::from(expected),
                    form.right,
                    got
                );
                let reason: String = reason.chars().take(200).collect();
                assert!(failures.insert(form.bare, reason).is_none());
            }
        }
    }

    let listed: BTreeSet<_> = SKIPPED
        .iter()
        .flat_map(|(_, pairs)| pairs.iter().map(|pair| pair.to_string()))
        .collect();
    assert_eq!(
        skipped, listed,
        "SKIPPED is out of date: these pairs have no operands that tell the groupings apart"
    );
    check_known_failures(
        "FHIRPath precedence",
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fhirpath-precedence/known-failures.json"),
        run,
        failures,
    );
}
