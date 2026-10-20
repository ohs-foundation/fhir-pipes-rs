//! What both engines need to know about a lowBoundary()/highBoundary() base.

use octofhir_fhirpath::ExpressionNode;

/// The FHIR temporal type a boundary operates on, inferred from an
/// `ofType(T)` on the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundaryType {
    Date,
    DateTime,
    Time,
    Unknown,
}

/// The temporal type of `object`, from an `ofType(T)` on it or on the
/// element it is read from (`value.ofType(dateTime).x`).
pub(crate) fn boundary_hint(object: &ExpressionNode) -> BoundaryType {
    match object {
        ExpressionNode::MethodCall(m) if m.method == "ofType" => {
            let name = match m.arguments.first() {
                Some(ExpressionNode::Identifier(n)) => n.name.to_lowercase(),
                Some(ExpressionNode::TypeInfo(t)) => t.name.to_lowercase(),
                _ => return BoundaryType::Unknown,
            };
            match name.as_str() {
                "datetime" | "instant" => BoundaryType::DateTime,
                "date" => BoundaryType::Date,
                "time" => BoundaryType::Time,
                _ => BoundaryType::Unknown,
            }
        }
        ExpressionNode::PropertyAccess(p) => boundary_hint(&p.object),
        ExpressionNode::Parenthesized(e) => boundary_hint(e),
        _ => BoundaryType::Unknown,
    }
}

/// `s` with its first letter in upper case (`dateTime` → `DateTime`), as a
/// choice element's JSON name spells its type.
pub(crate) fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
