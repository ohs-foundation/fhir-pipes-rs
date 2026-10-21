//! `lowBoundary()` and `highBoundary()` by the spec's precision rules, over
//! decimals and partial dates, dateTimes and times.

use octofhir_fhirpath::{Collection, EvaluationContext, ExpressionNode, FhirPathValue};
use serde_json::Value;

use crate::boundary_type::{BoundaryType, boundary_hint};
use crate::{Error, Result};

use super::evaluator::{Evaluator, PreparedPath, collection_to_json};

impl Evaluator {
    /// `lowBoundary()`/`highBoundary()` over the values of `base`.
    ///
    /// Values read from resource JSON reach the engine untyped (a dateTime
    /// is a string), so its own boundary functions cannot apply to them.
    /// Each value is written back as a FHIRPath literal of the type it
    /// stands for (`2010-10-10` under `ofType(dateTime)` becomes
    /// `@2010-10-10T`) and the engine evaluates the boundary of that literal.
    /// A value that is not a number, a Quantity or a well-formed date, time
    /// or dateTime has no boundary, so no data text ever reaches the
    /// expression.
    pub(super) async fn eval_boundary(
        &self,
        base: &PreparedPath,
        low: bool,
        hint: BoundaryType,
        precision: Option<&ExpressionNode>,
        ctx: &EvaluationContext,
    ) -> Result<Vec<Value>> {
        let digits = match precision {
            None => None,
            Some(node) => match self.eval_node(node, ctx).await?.iter().next() {
                Some(FhirPathValue::Integer(i, ..)) => Some(*i),
                None => return Ok(Vec::new()),
                Some(other) => {
                    return Err(Error::FhirPath(format!(
                        "lowBoundary()/highBoundary() precision must be an integer, got {}",
                        other.to_json_value()
                    )));
                }
            },
        };
        let function = if low { "lowBoundary" } else { "highBoundary" };
        let mut out = Vec::new();
        for value in Box::pin(self.eval_values(base, ctx)).await? {
            let Some(input) = boundary_input(&value, hint) else {
                continue;
            };
            let kind = input.kind();
            // The engine keeps a date, time or dateTime at its own precision
            // when none is given; the spec widens it to the type's full
            // precision.
            let digits = digits.or(kind.full_precision());
            let arg = digits.map(|d| d.to_string()).unwrap_or_default();
            let result = match input {
                BoundaryInput::Typed(_) => {
                    let focus = ctx.create_child_context(Collection::single(value.clone()));
                    focus.set_this(value.clone());
                    self.engine
                        .evaluate(&format!("$this.{function}({arg})"), &focus)
                        .await
                }
                BoundaryInput::Literal(literal, _) => {
                    self.engine
                        .evaluate(&format!("{literal}.{function}({arg})"), &ctx.nest())
                        .await
                }
            }
            .map_err(|e| Error::FhirPath(e.to_string()))?;
            out.extend(
                collection_to_json(&result.value)
                    .into_iter()
                    .map(|v| match v {
                        // A time's JSON form has no leading `T`.
                        Value::String(t) if kind == BoundaryType::Time => {
                            Value::String(t.trim_start_matches('T').to_string())
                        }
                        v => v,
                    }),
            );
        }
        Ok(out)
    }
}

/// If `node` is a `lowBoundary()`/`highBoundary()` call, the path that
/// evaluates its base (a node, or another boundary call) and applies the
/// boundary. `written` is the same call in the path as written, before the
/// `ofType` rewrite, from which the temporal hint is read.
pub(super) fn boundary_path(
    node: &ExpressionNode,
    written: Option<&ExpressionNode>,
) -> Option<PreparedPath> {
    let ExpressionNode::MethodCall(m) = node else {
        return None;
    };
    let low = match m.method.as_str() {
        "lowBoundary" => true,
        "highBoundary" => false,
        _ => return None,
    };
    let written = match written {
        Some(ExpressionNode::MethodCall(w)) if w.method == m.method => Some(w.object.as_ref()),
        _ => None,
    };
    let hint = written.map_or(BoundaryType::Unknown, boundary_hint);
    let base = boundary_path(&m.object, written)
        .unwrap_or_else(|| PreparedPath::Node(m.object.as_ref().clone()));
    let precision = m.arguments.first().cloned();
    Some(PreparedPath::Boundary(Box::new(base), low, hint, precision))
}

impl BoundaryType {
    /// Precision digits of the type's finest form: day, millisecond,
    /// millisecond. `None` for numbers, whose default the engine applies.
    fn full_precision(self) -> Option<i64> {
        match self {
            BoundaryType::Date => Some(8),
            BoundaryType::DateTime => Some(17),
            BoundaryType::Time => Some(9),
            BoundaryType::Unknown => None,
        }
    }
}

/// What a boundary is computed on: a value the engine has typed, or a
/// literal written for a value it has not.
enum BoundaryInput {
    Typed(BoundaryType),
    Literal(String, BoundaryType),
}

impl BoundaryInput {
    fn kind(&self) -> BoundaryType {
        match self {
            BoundaryInput::Typed(k) | BoundaryInput::Literal(_, k) => *k,
        }
    }
}

/// How to compute the boundary of `value`, or `None` if it has none.
///
/// Dates, times, dateTimes, integers and Quantities the engine has typed are
/// used as they are. A decimal is written as a literal when its JSON form
/// shows more digits after the point than the engine kept (a resource's
/// `1.0` reaches the engine as `1`, and the boundary depends on the digits
/// written). Strings, which is how the engine holds a resource's date, time
/// or dateTime, become a literal of the type `hint` names.
fn boundary_input(value: &FhirPathValue, hint: BoundaryType) -> Option<BoundaryInput> {
    let typed = |k| Some(BoundaryInput::Typed(k));
    match value {
        FhirPathValue::Integer(..) | FhirPathValue::Quantity { .. } => typed(BoundaryType::Unknown),
        FhirPathValue::Date(..) => typed(BoundaryType::Date),
        FhirPathValue::DateTime(..) => typed(BoundaryType::DateTime),
        FhirPathValue::Time(..) => typed(BoundaryType::Time),
        FhirPathValue::Decimal(d, ..) => {
            let engine = d.to_string();
            let text = decimal_text(&engine, &value.to_json_value());
            if text == engine {
                typed(BoundaryType::Unknown)
            } else {
                Some(BoundaryInput::Literal(signed(text), BoundaryType::Unknown))
            }
        }
        _ => match value.to_json_value() {
            Value::Number(n) => Some(BoundaryInput::Literal(
                signed(n.to_string()),
                BoundaryType::Unknown,
            )),
            Value::String(s) => {
                temporal_literal(&s, hint).map(|(l, k)| BoundaryInput::Literal(l, k))
            }
            _ => None,
        },
    }
}

/// The digits of a decimal: the engine's, or the JSON number's when it has
/// more after the point (a resource's `1.0` reaches the engine as `1`, and
/// the boundary depends on the digits written).
fn decimal_text(engine: &str, json: &Value) -> String {
    let fraction = |s: &str| s.split_once('.').map_or(0, |(_, f)| f.len());
    match json.as_number().map(|n| n.to_string()) {
        Some(j) if fraction(&j) > fraction(engine) => j,
        _ => engine.to_string(),
    }
}

/// A number literal, in parentheses when negative so the method binds to it.
fn signed(number: String) -> String {
    if number.starts_with('-') {
        format!("({number})")
    } else {
        number
    }
}

/// A date, dateTime or time string from a resource as a FHIRPath literal of
/// the type `hint` names, or that its form shows when `hint` is unknown.
/// `None` unless the string is well formed.
fn temporal_literal(s: &str, hint: BoundaryType) -> Option<(String, BoundaryType)> {
    let digits = |t: &str, n: usize| t.len() == n && t.bytes().all(|b| b.is_ascii_digit());
    let is_time = |t: &str| {
        let (hms, frac) = t.split_once('.').unwrap_or((t, "0"));
        let parts: Vec<&str> = hms.split(':').collect();
        !parts.is_empty()
            && parts.len() <= 3
            && parts.iter().all(|p| digits(p, 2))
            && frac.bytes().all(|b| b.is_ascii_digit())
    };
    let is_date = |t: &str| {
        let parts: Vec<&str> = t.split('-').collect();
        digits(parts[0], 4) && parts.len() <= 3 && parts[1..].iter().all(|p| digits(p, 2))
    };
    let is_datetime = |t: &str| match t.split_once('T') {
        None => is_date(t),
        Some((date, time)) => {
            let time = time.trim_end_matches('Z');
            let time = match time.rfind(['+', '-']) {
                Some(i) => {
                    let (clock, offset) = time.split_at(i);
                    let offset = &offset[1..];
                    if !(offset.len() == 5 && is_time(offset)) {
                        return false;
                    }
                    clock
                }
                None => time,
            };
            is_date(date) && date.len() == 10 && is_time(time)
        }
    };
    let ty = match hint {
        BoundaryType::Unknown if is_time(s) => BoundaryType::Time,
        BoundaryType::Unknown if s.contains('T') => BoundaryType::DateTime,
        BoundaryType::Unknown => BoundaryType::Date,
        known => known,
    };
    match ty {
        BoundaryType::Time if is_time(s) => Some((format!("@T{s}"), ty)),
        BoundaryType::Date if is_date(s) => Some((format!("@{s}"), ty)),
        BoundaryType::DateTime if is_datetime(s) => {
            let t = if s.contains('T') { "" } else { "T" };
            Some((format!("@{s}{t}"), ty))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    use crate::eval::execute;
    use crate::eval::tests::view;
    use serde_json::json;

    #[test]
    fn boundaries_follow_the_precision_argument() {
        let obs = json!({
            "resourceType": "Observation", "id": "o1",
            "valueQuantity": { "value": 1.587 },
            "effectiveDateTime": "2010-10-10",
            "issued": "not a date"
        });
        let column = |path: &str| {
            let v = view(json!({
                "resource": "Observation",
                "select": [{ "column": [{ "name": "b", "path": path, "collection": true }] }]
            }));
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(execute(&v, std::slice::from_ref(&obs)))
                .unwrap()
                .data[0][0]
                .clone()
        };
        assert_eq!(
            column("value.ofType(Quantity).value.lowBoundary(2)"),
            json!([1.58])
        );
        assert_eq!(
            column("value.ofType(Quantity).value.highBoundary(2)"),
            json!([1.59])
        );
        // A precision out of range has no boundary.
        assert_eq!(
            column("value.ofType(Quantity).value.lowBoundary(-1)"),
            json!([])
        );
        assert_eq!(
            column("value.ofType(Quantity).value.lowBoundary(32)"),
            json!([])
        );
        // A resource's dateTime string, read as the type the path names.
        assert_eq!(
            column("effective.ofType(dateTime).lowBoundary()"),
            json!(["2010-10-10T00:00:00.000+14:00"])
        );
        assert_eq!(
            column("effective.ofType(dateTime).highBoundary(6)"),
            json!(["2010-10"])
        );
        // Text that is not a date has no boundary, and never reaches the
        // expression the engine evaluates.
        assert_eq!(column("issued.lowBoundary()"), json!([]));
        assert_eq!(column("@T10:30.lowBoundary(9)"), json!(["10:30:00.000"]));
    }
}
