//! What a `repeat` path may be, for both engines.

use octofhir_fhirpath::ExpressionNode;

use crate::Error;

/// A `repeat` path must read child elements of the node it is applied to.
/// Each step then goes one level down the resource's JSON, so the traversal
/// ends; a path that starts from the node itself (`$this`), the resource
/// (`%resource`, or the resource type by name) or a literal could come back
/// to where it began and never end.
pub(crate) fn check(path: &str, ast: &ExpressionNode, resource: &str) -> Result<(), Error> {
    let refuse = |start: &str| {
        Err(Error::InvalidViewDefinition(format!(
            "repeat path {path:?} starts from {start}; a repeat path must read child \
             elements of the node it is applied to, or it can loop for ever"
        )))
    };
    match ast {
        ExpressionNode::Union(u) => {
            check(path, &u.left, resource)?;
            check(path, &u.right, resource)
        }
        ExpressionNode::PropertyAccess(p) => check(path, &p.object, resource),
        ExpressionNode::MethodCall(m) => check(path, &m.object, resource),
        ExpressionNode::Filter(f) => check(path, &f.base, resource),
        ExpressionNode::IndexAccess(i) => check(path, &i.object, resource),
        ExpressionNode::Parenthesized(e) => check(path, e, resource),
        ExpressionNode::TypeCast(c) => check(path, &c.expression, resource),
        ExpressionNode::Variable(v) => refuse(&format!("${}", v.name.trim_start_matches('$'))),
        ExpressionNode::Identifier(n) if n.name == resource => refuse(&format!("the {resource}")),
        ExpressionNode::Literal(_) => refuse("a literal"),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octofhir_fhirpath::parse_ast;

    fn checked(path: &str) -> Result<(), Error> {
        check(path, &parse_ast(path).unwrap(), "Questionnaire")
    }

    #[test]
    fn child_paths_are_allowed_and_paths_back_to_the_start_are_not() {
        for ok in [
            "item",
            "item.where(type = 'group')",
            "answer.item | item",
            "item[0]",
        ] {
            checked(ok).unwrap();
        }
        for bad in [
            "$this",
            "%resource.item",
            "Questionnaire.item",
            "item | $this",
            "'x'",
        ] {
            assert!(checked(bad).is_err(), "{bad}");
        }
    }
}
