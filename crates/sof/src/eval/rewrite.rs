//! Reading and rewriting a path before the engine compiles it: `%context`
//! renamed, the names `defineVariable()` binds, and `ofType()` over a choice
//! element turned into the element's JSON name.

use octofhir_fhirpath::ExpressionNode;

use crate::boundary_type::capitalize_first;
use crate::fhir_model;
use crate::lexer::{Kind, Token, tokens, variable_name};

use super::evaluator::CONTEXT_VAR;

/// `path` with each `%context` (or ``%`context` ``) renamed to
/// [`CONTEXT_VAR`], or `None` if it has none. Run after constant
/// substitution, so a view constant named `context` has already been
/// replaced.
pub(super) fn rename_context(path: &str) -> Option<String> {
    let toks = tokens(path);
    let is_context = |t: &Token| {
        let text = &path[t.span.clone()];
        t.kind == Kind::Variable && text.starts_with('%') && variable_name(text) == "context"
    };
    if !toks.iter().any(is_context) {
        return None;
    }
    let mut out = String::with_capacity(path.len() + 8);
    for t in &toks {
        if is_context(t) {
            out.push('%');
            out.push_str(CONTEXT_VAR);
        } else {
            out.push_str(&path[t.span.clone()]);
        }
    }
    Some(out)
}

/// FHIRPath environment variables the engine resolves itself, which constant
/// substitution leaves in place. `%factory` is FHIR's type factory.
pub(super) fn is_environment_variable(name: &str) -> bool {
    matches!(
        name,
        "resource" | "rootResource" | "context" | "ucum" | "sct" | "loinc" | "factory"
    ) || name.starts_with("vs-")
        || name.starts_with("ext-")
}

/// The names a path binds with `defineVariable('name', ...)`, so that its
/// `%name` references are not taken for undefined constants. Empty if the
/// path does not parse; compiling it then reports the error. `None` if a
/// name is computed (`defineVariable(%param, ...)`), when any `%name` may be
/// bound.
pub(super) fn defined_variables(path: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut computed = false;
    if let Ok(ast) = octofhir_fhirpath::parse_ast(path) {
        visit(&ast, &mut |node| {
            let (name, args) = match node {
                ExpressionNode::MethodCall(m) => (&m.method, &m.arguments),
                ExpressionNode::FunctionCall(f) => (&f.name, &f.arguments),
                _ => return,
            };
            if name != "defineVariable" {
                return;
            }
            match args.first() {
                Some(ExpressionNode::Literal(l)) => {
                    if let octofhir_fhirpath::LiteralValue::String(s) = &l.value {
                        names.push(s.clone());
                    }
                }
                _ => computed = true,
            }
        });
    }
    (!computed).then_some(names)
}

/// Whether the FHIRPath `path` calls a function named in `names`, as a
/// function (`now()`) or a method (`x.now()`), however the call is written
/// (comments, a delimited name). `None` when the path does not parse.
pub fn calls_any(path: &str, names: &[&str]) -> Option<bool> {
    let ast = octofhir_fhirpath::parse_ast(path).ok()?;
    let mut found = false;
    visit(&ast, &mut |node| {
        let name = match node {
            ExpressionNode::MethodCall(m) => &m.method,
            ExpressionNode::FunctionCall(f) => &f.name,
            _ => return,
        };
        found |= names.contains(&name.as_str());
    });
    Some(found)
}

/// Call `f` on `node` and every node below it.
fn visit(node: &ExpressionNode, f: &mut impl FnMut(&ExpressionNode)) {
    f(node);
    match node {
        ExpressionNode::Literal(_)
        | ExpressionNode::Identifier(_)
        | ExpressionNode::Variable(_)
        | ExpressionNode::TypeInfo(_) => {}
        ExpressionNode::FunctionCall(n) => n.arguments.iter().for_each(|a| visit(a, f)),
        ExpressionNode::MethodCall(n) => {
            visit(&n.object, f);
            n.arguments.iter().for_each(|a| visit(a, f));
        }
        ExpressionNode::PropertyAccess(n) => visit(&n.object, f),
        ExpressionNode::IndexAccess(n) => {
            visit(&n.object, f);
            visit(&n.index, f);
        }
        ExpressionNode::BinaryOperation(n) => {
            visit(&n.left, f);
            visit(&n.right, f);
        }
        ExpressionNode::UnaryOperation(n) => visit(&n.operand, f),
        ExpressionNode::Lambda(n) => visit(&n.body, f),
        ExpressionNode::Collection(n) => n.elements.iter().for_each(|e| visit(e, f)),
        ExpressionNode::Parenthesized(e) => visit(e, f),
        ExpressionNode::TypeCast(n) => visit(&n.expression, f),
        ExpressionNode::Filter(n) => {
            visit(&n.base, f);
            visit(&n.condition, f);
        }
        ExpressionNode::Union(n) => {
            visit(&n.left, f);
            visit(&n.right, f);
        }
        ExpressionNode::TypeCheck(n) => visit(&n.expression, f),
        ExpressionNode::Path(n) => visit(&n.base, f),
    }
}

/// Rewrite `element.ofType(T)`, where `T` is a FHIR datatype, to the JSON
/// name a choice element has for that type: `value.ofType(Quantity)` →
/// `valueQuantity`, `value.ofType(string)` → `valueString`. The type name is
/// capitalized as the JSON name requires. The engine cannot map choice
/// elements itself: the model types a choice element by its JSON name
/// (`valueQuantity`), and `value` alone has no type (see `fhir_model`).
///
/// `ofType` on an element that is not a choice element
/// (`name.ofType(HumanName)`, `gender.ofType(code)`) is left to the engine,
/// which knows the element's type from the model. Whether an element is a
/// choice element comes from the FHIR model, for the type or backbone element
/// the path reaches it from ([`step_owner`]): `contact.name` on a Patient is
/// `Patient.contact`'s `name`, not a choice element. `root` is where the
/// path starts: the view's resource type, or a `forEach` focus
/// ([`focus_owner`]). When the path does not show where the element is (it
/// follows a variable or an operator), any choice element of that name
/// counts.
///
/// The rewrite works on tokens, so string literals, backtick-quoted names and
/// comments are left alone. `ofType` with a resource type
/// (`contained.ofType(Practitioner)`), on a base that is not an element name
/// (`first().ofType(string)`), or with a qualified type is left to the engine.
pub(super) fn rewrite_of_type(path: &str, root: Option<&str>) -> String {
    let toks = tokens(path);
    // The tokens that are not whitespace or comments.
    let sig: Vec<&Token> = toks
        .iter()
        .filter(|t| !matches!(t.kind, Kind::Space | Kind::Comment))
        .collect();
    let text = |t: &Token| &path[t.span.clone()];
    let mut out = String::with_capacity(path.len());
    let mut copied = 0;
    let mut k = 0;
    while let Some(&[element, dot, of_type, open, ty, close]) = sig.get(k..k + 6) {
        if element.kind == Kind::Ident
            && text(dot) == "."
            && text(of_type) == "ofType"
            && text(open) == "("
            && ty.kind == Kind::Ident
            && text(close) == ")"
            && fhir_model::is_datatype(text(ty))
            // On an element that is not a choice element the engine, which
            // knows the element's type from the model, answers it
            // (`name.ofType(HumanName)`, `gender.ofType(code)`).
            && is_choice(path, &sig, k, root)
        {
            let (element, ty_name) = (text(element), text(ty));
            out.push_str(&path[copied..sig[k].span.start]);
            out.push_str(element);
            out.push_str(&capitalize_first(ty_name));
            copied = close.span.end;
            k += 6;
        } else {
            k += 1;
        }
    }
    out.push_str(&path[copied..]);
    out
}

/// Whether the element at significant token `k` is a choice element (see
/// [`rewrite_of_type`]).
fn is_choice(path: &str, sig: &[&Token], k: usize, root: Option<&str>) -> bool {
    let element = &path[sig[k].span.clone()];
    match step_owner(path, sig, k, root) {
        Some(owner) => fhir_model::is_choice_element(&owner, element),
        None => fhir_model::is_choice_anywhere(element),
    }
}

/// The type or backbone element whose element is at significant token `k`:
/// followed through the chain of steps before it (`contact.name`,
/// `name.where(use = 'official').family`, `value.ofType(Range).low`) from
/// `root`, when the chain starts the path, or from a type name the chain
/// starts with (`Patient.contact`). `None` when the chain starts elsewhere
/// (`$this.value`, after an operator) or reaches an element the model has no
/// type for.
fn step_owner(path: &str, sig: &[&Token], k: usize, root: Option<&str>) -> Option<String> {
    let text = |t: &Token| &path[t.span.clone()];
    if k == 0 {
        return root.map(str::to_string);
    }
    if text(sig[k - 1]) != "." || k < 2 {
        return None;
    }
    let start = chain_start(path, sig, k - 2)?;
    walk_chain(
        path,
        &sig[start..k - 1],
        (start == 0).then_some(root).flatten(),
    )
}

/// The type or backbone element that the items `path` selects have
/// (`contact` on a Patient: `Patient.contact`), when the whole path is a
/// chain of steps; else `None`.
pub(super) fn focus_owner(path: &str, root: Option<&str>) -> Option<String> {
    let toks = tokens(path);
    let sig: Vec<&Token> = toks
        .iter()
        .filter(|t| !matches!(t.kind, Kind::Space | Kind::Comment))
        .collect();
    if chain_start(path, &sig, sig.len().checked_sub(1)?)? != 0 {
        return None;
    }
    walk_chain(path, &sig, root)
}

/// Index of the first token of the chain of steps (`a.b(...).c`) that ends
/// at significant token `end`, an identifier or a call's `)`.
fn chain_start(path: &str, sig: &[&Token], mut end: usize) -> Option<usize> {
    let text = |t: &Token| &path[t.span.clone()];
    loop {
        let first = if text(sig[end]) == ")" {
            // Back to the matching `(` and the function name before it.
            let mut depth = 0;
            let open = (0..=end).rev().find(|&i| {
                match text(sig[i]) {
                    ")" => depth += 1,
                    "(" => depth -= 1,
                    _ => {}
                }
                depth == 0
            })?;
            (open > 0 && sig[open - 1].kind == Kind::Ident).then(|| open - 1)?
        } else if sig[end].kind == Kind::Ident {
            end
        } else {
            return None;
        };
        if first >= 2 && text(sig[first - 1]) == "." {
            end = first - 2;
        } else {
            return Some(first);
        }
    }
}

/// The owner reached by the chain of steps `chain`, starting from `root`, or
/// from the type name the chain starts with. Calls that keep the items' type
/// (`where`, `first`, ...) are passed over, and `ofType(T)` gives `T`.
fn walk_chain(path: &str, chain: &[&Token], root: Option<&str>) -> Option<String> {
    let text = |t: &Token| &path[t.span.clone()];
    let first = text(*chain.first()?);
    let is_call = |i: usize| chain.get(i + 1).is_some_and(|t| text(t) == "(");
    let (mut owner, mut i) = if first.starts_with(|c: char| c.is_ascii_uppercase())
        && fhir_model::is_type(first)
        && !is_call(0)
    {
        (first.to_string(), 1)
    } else {
        (root?.to_string(), 0)
    };
    while i < chain.len() {
        let name = text(chain[i]);
        if name == "." {
            i += 1;
        } else if is_call(i) {
            let mut depth = 0;
            let close = (i + 1..chain.len()).find(|&j| {
                match text(chain[j]) {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    _ => {}
                }
                depth == 0
            })?;
            match name {
                "where" | "first" | "last" | "tail" | "skip" | "take" | "distinct" | "single"
                | "trace" => {}
                "ofType" if close == i + 3 && fhir_model::is_type(text(chain[i + 2])) => {
                    owner = text(chain[i + 2]).to_string();
                }
                _ => return None,
            }
            i = close + 1;
        } else {
            // A choice element by its own name (`value`) has no type until
            // an `ofType(T)` after it gives one.
            let of_type_next = chain.get(i + 1).is_some_and(|t| text(t) == ".")
                && chain.get(i + 2).is_some_and(|t| text(t) == "ofType");
            owner = match fhir_model::element_owner(&owner, name) {
                Some(next) => next.to_string(),
                None if of_type_next => String::new(),
                None => return None,
            };
            i += 1;
        }
    }
    (!owner.is_empty()).then_some(owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_type_is_rewritten_only_for_datatypes_on_element_names() {
        for (path, want) in [
            ("value.ofType(Quantity).value", "valueQuantity.value"),
            ("a.value . ofType ( dateTime )", "a.valueDateTime"),
            (
                "where(value.ofType(integer) > 1)",
                "where(valueInteger > 1)",
            ),
            // Left to the engine.
            (
                "contained.ofType(Practitioner)",
                "contained.ofType(Practitioner)",
            ),
            ("first().ofType(string)", "first().ofType(string)"),
            ("$this.ofType(string)", "$this.ofType(string)"),
            ("value.ofType(FHIR.string)", "value.ofType(FHIR.string)"),
            ("'a.ofType(string)'", "'a.ofType(string)'"),
            ("id // value.ofType(string)", "id // value.ofType(string)"),
        ] {
            assert_eq!(rewrite_of_type(path, None), want, "{path}");
        }
    }

    #[test]
    fn of_type_is_rewritten_only_on_choice_elements() {
        for (path, root, want) in [
            // The parent is named in the path, or is where it starts.
            (
                "Observation.value.ofType(Quantity)",
                None,
                "Observation.valueQuantity",
            ),
            (
                "value.ofType(Quantity)",
                Some("Observation"),
                "valueQuantity",
            ),
            (
                "Patient.name.ofType(HumanName)",
                None,
                "Patient.name.ofType(HumanName)",
            ),
            (
                "name.ofType(HumanName)",
                Some("Patient"),
                "name.ofType(HumanName)",
            ),
            // Deeper, the model follows the path to the element's parent.
            (
                "component.value.ofType(Quantity)",
                Some("Observation"),
                "component.valueQuantity",
            ),
            (
                "contact.name.ofType(HumanName)",
                Some("Patient"),
                "contact.name.ofType(HumanName)",
            ),
            (
                "Patient.contact.name.ofType(HumanName)",
                None,
                "Patient.contact.name.ofType(HumanName)",
            ),
            (
                "name.where(use = 'official').family.ofType(string)",
                Some("Patient"),
                "name.where(use = 'official').family.ofType(string)",
            ),
            (
                "value.ofType(Range).low.ofType(Quantity)",
                Some("Observation"),
                "valueRange.low.ofType(Quantity)",
            ),
            // A forEach focus (`component` on an Observation).
            (
                "value.ofType(Quantity)",
                Some("Observation.component"),
                "valueQuantity",
            ),
            // Where the path does not show the parent, any choice element of
            // that name counts: `name` is one in
            // ImplementationGuide.definition.page.
            (
                "component.value.ofType(Quantity)",
                None,
                "component.valueQuantity",
            ),
            (
                "contact.name.ofType(HumanName)",
                None,
                "contact.nameHumanName",
            ),
            (
                "$this.value.ofType(Quantity)",
                Some("Observation"),
                "$this.valueQuantity",
            ),
            (
                "contact.telecom.ofType(ContactPoint)",
                None,
                "contact.telecom.ofType(ContactPoint)",
            ),
            // So is a primitive type: the engine answers from the model.
            (
                "Observation.code.ofType(code)",
                None,
                "Observation.code.ofType(code)",
            ),
            (
                "Patient.gender.ofType(code)",
                None,
                "Patient.gender.ofType(code)",
            ),
        ] {
            assert_eq!(rewrite_of_type(path, root), want, "{path}");
        }
    }

    #[test]
    fn a_for_each_focus_has_its_owner() {
        assert_eq!(
            focus_owner("contact", Some("Patient")).as_deref(),
            Some("Patient.contact")
        );
        assert_eq!(
            focus_owner("name", Some("Patient")).as_deref(),
            Some("HumanName")
        );
        assert_eq!(
            focus_owner("name.where(use = 'official')", Some("Patient")).as_deref(),
            Some("HumanName")
        );
        assert_eq!(
            focus_owner("value.ofType(Quantity)", Some("Observation")).as_deref(),
            Some("Quantity")
        );
        assert_eq!(
            focus_owner("item", Some("Questionnaire.item")).as_deref(),
            Some("Questionnaire.item")
        );
        // Not a chain of steps, a primitive, or nothing to start from.
        assert_eq!(focus_owner("name | contact.name", Some("Patient")), None);
        assert_eq!(focus_owner("name.given", Some("Patient")), None);
        assert_eq!(focus_owner("contact", None), None);
        assert_eq!(
            focus_owner("Patient.contact", None).as_deref(),
            Some("Patient.contact")
        );
    }

    #[test]
    fn context_is_renamed_outside_literals() {
        assert_eq!(
            rename_context("%context.a | %`context`.b | '%context' | %contextual").as_deref(),
            Some("%_sofContext.a | %_sofContext.b | '%context' | %contextual")
        );
        assert_eq!(rename_context("%resource.id // %context"), None);
    }
}
