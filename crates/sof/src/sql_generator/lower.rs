//! The AST→SQL lowering engine: turns a parsed FHIRPath expression into a
//! JSONB-collection SQL expression, and builds the per-level row plans.

use std::cell::Cell;
use std::collections::HashMap;

use octofhir_fhirpath::{BinaryOperator, ExpressionNode, LiteralValue, UnaryOperator, parse_ast};

use crate::Error;
use crate::column::ColumnType;
use crate::view_definition::{Column, SelectColumn};

use super::boundary::{
    BoundaryType, boundary_hint, capitalize_first, date_bound, datetime_bound, time_bound,
};
use super::constants::substitute_constants;
use super::ddl::Dialect;
use super::{Plan, PlanColumn};

/// The AST→SQL lowering context for a single `generate` call.
pub(super) struct Lower {
    resource_type: String,
    /// Constant name → FHIRPath literal text, pre-rendered for substitution.
    constants: HashMap<String, String>,
    seq: Cell<usize>,
    /// SQL expression yielding the current `%rowIndex` (0-based). "0" at the top
    /// level; a `forEach`/`forEachOrNull` sets it to `<alias>.ord - 1` for the
    /// duration of that level (saved/restored around each `build_select`).
    row_idx: std::cell::RefCell<String>,
    /// SQL expression yielding the root resource JSONB (e.g. `base.resource`).
    /// Fragment (`#id`) references resolve against its `contained[]`.
    root: String,
    /// Target SQL dialect for the emitted JSON expressions.
    dialect: Dialect,
}

impl Lower {
    pub(super) fn new(
        resource_type: String,
        constants: HashMap<String, String>,
        root: String,
        dialect: Dialect,
    ) -> Self {
        Self {
            resource_type,
            constants,
            seq: Cell::new(0),
            row_idx: std::cell::RefCell::new("0".to_string()),
            root,
            dialect,
        }
    }

    fn fresh(&self) -> usize {
        let v = self.seq.get();
        self.seq.set(v + 1);
        v
    }

    /// Build the UNION ALL branches for a select node, given the join prefix and
    /// the current focus item (`ctx`, a JSONB scalar SQL expression).
    pub(super) fn build_select(
        &self,
        select: &SelectColumn,
        prefix: &[String],
        ctx: &str,
    ) -> Result<Vec<Plan>, Error> {
        // `%rowIndex` is scoped to this level: save the enclosing value, restore
        // it before returning so siblings/unionAll branches inherit correctly.
        let saved_ri = self.row_idx.borrow().clone();

        // forEach / forEachOrNull / repeat establish a new focus (and a new
        // %rowIndex) for this level; they are mutually exclusive (sql-expressions).
        let (joins, ctx2): (Vec<String>, String) = if !select.repeat.is_empty() {
            self.repeat_join(&select.repeat, prefix, ctx)?
        } else if let Some(p) = &select.for_each {
            self.for_each_join(p, prefix, ctx, false)?
        } else if let Some(p) = &select.for_each_or_null {
            self.for_each_join(p, prefix, ctx, true)?
        } else {
            (prefix.to_vec(), ctx.to_string())
        };

        let plans = self.build_level(select, &joins, &ctx2);

        *self.row_idx.borrow_mut() = saved_ri;
        plans
    }

    /// Lower a select's own columns, nested selects and unionAll branches against
    /// an already-established focus and `%rowIndex` (set by the caller).
    fn build_level(
        &self,
        select: &SelectColumn,
        joins: &[String],
        ctx2: &str,
    ) -> Result<Vec<Plan>, Error> {
        // This node's own columns, evaluated against the focus.
        let mut own = Vec::new();
        if let Some(cols) = &select.column {
            for col in cols {
                own.push(self.lower_column(col, ctx2)?);
            }
        }

        let mut plans = vec![Plan {
            joins: joins.to_vec(),
            columns: own,
        }];

        // Nested selects cross-join with the accumulated plans.
        for child in &select.select {
            let mut next = Vec::new();
            for p in &plans {
                for cp in self.build_select(child, &p.joins, ctx2)? {
                    next.push(p.cross(&cp));
                }
            }
            plans = next;
        }

        // unionAll branches are row alternatives, each cross-joined with the
        // sibling columns/selects of this level.
        if let Some(branches) = &select.union_all {
            let mut unioned = Vec::new();
            for p in &plans {
                for branch in branches {
                    for bp in self.build_select(branch, &p.joins, ctx2)? {
                        unioned.push(p.cross(&bp));
                    }
                }
            }
            plans = unioned;
        }

        Ok(plans)
    }

    fn for_each_join(
        &self,
        path: &str,
        prefix: &[String],
        ctx: &str,
        or_null: bool,
    ) -> Result<(Vec<String>, String), Error> {
        let path = self.substitute(path)?;
        let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
        let coll = self.coll(&ast, ctx)?;
        let alias = format!("fe{}", self.fresh());
        // WITH ORDINALITY (or DuckDB's zipped unnest) exposes a 1-based
        // position; %rowIndex is 0-based.
        let src = self.dialect.elements_ord_table(&coll, &alias);
        let join = if or_null {
            format!("LEFT JOIN LATERAL {src} ON true")
        } else {
            format!("CROSS JOIN LATERAL {src}")
        };
        let mut joins = prefix.to_vec();
        joins.push(join);
        // For forEachOrNull, the null row carries %rowIndex = 0 (per spec).
        let ri = if or_null {
            format!("coalesce(({alias}.ord - 1), 0)")
        } else {
            format!("({alias}.ord - 1)")
        };
        *self.row_idx.borrow_mut() = ri;
        Ok((joins, format!("{alias}.value")))
    }

    /// Lower a `repeat` directive to a lateral, recursive traversal. The recursive
    /// CTE re-applies the repeat path(s) to every reached node (the focus itself
    /// excluded). A `pathkey` array of per-level ordinals orders the result in
    /// preorder, and `%rowIndex` is its 0-based position in that flattened list.
    fn repeat_join(
        &self,
        paths: &[String],
        prefix: &[String],
        ctx: &str,
    ) -> Result<(Vec<String>, String), Error> {
        let asts = paths
            .iter()
            .map(|p| {
                let s = self.substitute(p)?;
                parse_ast(&s).map_err(|e| Error::FhirPath(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let n = self.fresh();
        let cte = format!("_rep{n}");
        let alias = format!("rep{n}");
        let base = self.repeat_expand(&asts, ctx)?;
        let step = self.repeat_expand(&asts, &format!("{cte}.value"))?;

        let icast = self.dialect.int_cast();
        // List literal and concat differ: Postgres `ARRAY[k]::bigint[]` / `a || b`;
        // DuckDB `[k]::BIGINT[]` / `list_concat(a, b)`.
        let (path0, path_step) = if matches!(self.dialect, Dialect::DuckDb) {
            (
                format!("[_b{n}.key]::{icast}[]"),
                format!("list_concat({cte}.pathkey, [_s{n}.key])"),
            )
        } else {
            (
                format!("ARRAY[_b{n}.key]::{icast}[]"),
                format!("{cte}.pathkey || _s{n}.key"),
            )
        };
        // DuckDB cannot alias a CTE's columns with `cte(value, pathkey)` AND it
        // needs the recursive subquery shaped slightly differently, but the
        // Postgres form below is also accepted by DuckDB, so share it.
        // The recursive CTE lives inside a LATERAL subquery so its base case can
        // reference the enclosing focus `ctx`.
        let lateral = format!(
            "CROSS JOIN LATERAL (\
             WITH RECURSIVE {cte}(value, pathkey) AS (\
             SELECT _b{n}.value, {path0} FROM ({base}) AS _b{n} \
             UNION ALL \
             SELECT _s{n}.value, {path_step} \
             FROM {cte}, LATERAL ({step}) AS _s{n}) \
             SELECT value, (row_number() OVER (ORDER BY pathkey) - 1)::{icast} AS ridx \
             FROM {cte}) AS {alias}(value, ridx)"
        );
        let mut joins = prefix.to_vec();
        joins.push(lateral);
        *self.row_idx.borrow_mut() = format!("{alias}.ridx");
        Ok((joins, format!("{alias}.value")))
    }

    /// One traversal step: every node reached by applying each repeat path to
    /// `ctx`, tagged with an ordering `key` of `path_rank * 1e9 + element_ord` so
    /// that path order then element order is preserved.
    fn repeat_expand(&self, asts: &[ExpressionNode], ctx: &str) -> Result<String, Error> {
        let icast = self.dialect.int_cast();
        let mut parts = Vec::new();
        for (i, ast) in asts.iter().enumerate() {
            let coll = self.coll(ast, ctx)?;
            let m = self.fresh();
            let alias = format!("_e{m}");
            let src = self.dialect.elements_ord_table(&coll, &alias);
            parts.push(format!(
                "SELECT {alias}.value AS value, ({i}::{icast} * 1000000000 + {alias}.ord) AS key \
                 FROM {src}"
            ));
        }
        Ok(parts.join(" UNION ALL "))
    }

    fn lower_column(&self, col: &Column, ctx: &str) -> Result<PlanColumn, Error> {
        let path = self.substitute(&col.path)?;
        let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
        let coll = self.coll(&ast, ctx)?;
        let collection = col.collection.unwrap_or(false);
        // An `ansi/type` tag explicitly overrides the inferred column type.
        let ty = if let Some(ansi) = crate::eval::ansi_type_tag(col) {
            ColumnType::from_ansi_type(ansi)
        } else {
            col.col_type
                .as_deref()
                .map(ColumnType::from_fhir_type)
                .unwrap_or(ColumnType::String)
        };

        let (expr, col_type) = if collection {
            (coll, ColumnType::Json)
        } else {
            (self.scalar_col(&coll, ty), ty)
        };
        Ok(PlanColumn {
            name: col.name.clone(),
            expr,
            col_type,
        })
    }

    /// Extract a single value from a collection, applying the column's type cast.
    /// A collection of more than one element raises an error (the spec's
    /// "expected a single value" case for non-collection columns).
    fn scalar_col(&self, coll: &str, ty: ColumnType) -> String {
        let txt = self.dialect.scalar_text("_e");
        let inner = match ty {
            ColumnType::Integer | ColumnType::Integer64 => {
                format!("{txt}::{}", self.dialect.int_cast())
            }
            ColumnType::Decimal => format!("{txt}::{}", self.dialect.num_cast()),
            ColumnType::Boolean => format!("{txt}::{}", self.dialect.bool_cast()),
            ColumnType::Json => "_e".to_string(),
            _ => txt,
        };
        let n = self.fresh();
        let src = self.dialect.elements_table(coll, &format!("_c{n}"), "_e");
        format!("(SELECT {inner} FROM {src})")
    }

    // --- Collection lowering: every node evaluates to a JSONB array. ---

    fn coll(&self, node: &ExpressionNode, ctx: &str) -> Result<String, Error> {
        match node {
            ExpressionNode::Literal(l) => {
                Ok(self.dialect.build_array1(&self.literal_jsonb(&l.value)?))
            }
            ExpressionNode::Identifier(n) => {
                if n.name == self.resource_type {
                    Ok(self.dialect.build_array1(ctx))
                } else {
                    Ok(self.nav(&self.dialect.build_array1(ctx), &n.name))
                }
            }
            ExpressionNode::Variable(v) => {
                if v.name == "this" || v.name == "$this" {
                    Ok(self.dialect.build_array1(ctx))
                } else if v.name == "rowIndex" {
                    let ri = self.row_idx.borrow();
                    let icast = self.dialect.int_cast();
                    Ok(self
                        .dialect
                        .build_array1(&self.dialect.to_json_scalar(&format!("({ri})::{icast}"))))
                } else {
                    Err(Error::InvalidPath(format!(
                        "unsupported variable ${}",
                        v.name
                    )))
                }
            }
            ExpressionNode::PropertyAccess(p) => {
                let base = self.coll(&p.object, ctx)?;
                Ok(self.nav(&base, &p.property))
            }
            ExpressionNode::IndexAccess(i) => {
                let base = self.coll(&i.object, ctx)?;
                let idx = self.int_literal(&i.index)?;
                Ok(self.index(&base, idx))
            }
            ExpressionNode::MethodCall(m) => self.method(&m.object, &m.method, &m.arguments, ctx),
            ExpressionNode::FunctionCall(f) => self.func_root(&f.name, &f.arguments, ctx),
            ExpressionNode::Filter(fl) => {
                let base = self.coll(&fl.base, ctx)?;
                self.where_fn(&base, &fl.condition)
            }
            ExpressionNode::Union(u) => {
                let l = self.coll(&u.left, ctx)?;
                let r = self.coll(&u.right, ctx)?;
                let n = self.fresh();
                let la = self.dialect.elements_table(&l, &format!("_ua{n}"), "_v");
                let ra = self.dialect.elements_table(&r, &format!("_ub{n}"), "_v");
                Ok(format!(
                    "(SELECT {} FROM (SELECT _v FROM {la} UNION ALL SELECT _v FROM {ra}) AS _uu{n})",
                    self.dialect.agg("_v")
                ))
            }
            ExpressionNode::Parenthesized(e) => self.coll(e, ctx),
            ExpressionNode::TypeCast(c) => self.coll(&c.expression, ctx),
            ExpressionNode::BinaryOperation(_) | ExpressionNode::UnaryOperation(_) => {
                Ok(self.dialect.build_array1(&self.value_jsonb(node, ctx)?))
            }
            ExpressionNode::Collection(c) => {
                let mut parts = Vec::new();
                for e in &c.elements {
                    parts.push(self.coll(e, ctx)?);
                }
                if parts.is_empty() {
                    return Ok(self.dialect.empty_array().to_string());
                }
                let n = self.fresh();
                let froms: Vec<String> = parts
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let src = self.dialect.elements_table(p, &format!("_cl{n}_{i}"), "_v");
                        format!("SELECT _v FROM {src}")
                    })
                    .collect();
                Ok(format!(
                    "(SELECT {} FROM ({}) AS _clu{n})",
                    self.dialect.agg("_v"),
                    froms.join(" UNION ALL ")
                ))
            }
            other => Err(Error::InvalidPath(format!(
                "unsupported FHIRPath expression: {}",
                other.node_type()
            ))),
        }
    }

    /// JSONB navigation that flattens collections: for every item in `coll`,
    /// take `item->prop`, unwrapping arrays into the result.
    fn nav(&self, coll: &str, prop: &str) -> String {
        let p = prop.replace('\'', "''");
        let n = self.fresh();
        if matches!(self.dialect, Dialect::DuckDb) {
            // A 3-branch CASE with an empty `[]::JSON[]` middle branch fails to
            // type-check in DuckDB; use a 2-branch CASE and filter NULL/JSON-null
            // in the outer WHERE instead.
            let item = format!("_item{n}");
            let v = format!("_v{n}");
            let outer = self.dialect.elements_table(coll, &format!("_i{n}"), &item);
            let is_arr = self.dialect.is_array(&format!("{item} -> '{p}'"));
            let not_null = self.dialect.is_json_null(&v);
            format!(
                "(SELECT {agg} FROM (\
                   SELECT unnest(CASE \
                     WHEN {is_arr} THEN json_extract({item}, '$.{p}[*]') \
                     ELSE [{item} -> '{p}'] \
                   END) AS {v} \
                   FROM {outer} \
                 ) AS _nav{n} WHERE {v} IS NOT NULL AND NOT ({not_null}))",
                agg = self.dialect.agg(&v),
            )
        } else {
            let item = "_item";
            let inner = format!(
                "CASE \
                   WHEN {is_arr} THEN {item} -> '{p}' \
                   WHEN {item} -> '{p}' IS NULL THEN {empty} \
                   WHEN {is_null} THEN {empty} \
                   ELSE {one} \
                 END",
                is_arr = self.dialect.is_array(&format!("{item} -> '{p}'")),
                is_null = self.dialect.is_json_null(&format!("{item} -> '{p}'")),
                empty = self.dialect.empty_array(),
                one = self.dialect.build_array1(&format!("{item} -> '{p}'")),
            );
            let outer = self.dialect.elements_table(coll, "_i", item);
            format!(
                "(SELECT {agg} \
                 FROM {outer} \
                 CROSS JOIN LATERAL jsonb_array_elements({inner}) AS _j(_v))",
                agg = self.dialect.agg("_v"),
            )
        }
    }

    fn index(&self, coll: &str, n: i64) -> String {
        // Parenthesise the `->` access: DuckDB binds `->` looser than `IS NULL`,
        // so `x -> n IS NULL` would parse as `x -> (n IS NULL)`.
        let at = format!("(({coll}) -> {n})");
        format!(
            "(CASE WHEN {at} IS NULL THEN {empty} ELSE {one} END)",
            empty = self.dialect.empty_array(),
            one = self.dialect.build_array1(&at),
        )
    }

    /// A function applied to the current context (`name(...)` with no object).
    fn func_root(&self, name: &str, args: &[ExpressionNode], ctx: &str) -> Result<String, Error> {
        let seed = self.dialect.build_array1(ctx);
        self.apply_on_coll(name, &seed, args, ctx)
    }

    /// A method call `object.name(args)`.
    fn method(
        &self,
        object: &ExpressionNode,
        name: &str,
        args: &[ExpressionNode],
        ctx: &str,
    ) -> Result<String, Error> {
        match name {
            "ofType" => self.of_type(object, args.first(), ctx),
            "not" => Ok(self.dialect.build_array1(
                &self
                    .dialect
                    .to_json_scalar(&format!("NOT ({})", self.bool(object, ctx)?)),
            )),
            "lowBoundary" | "highBoundary" => {
                let hint = boundary_hint(object);
                let oc = self.coll(object, ctx)?;
                Ok(self.boundary(&oc, name == "lowBoundary", hint))
            }
            _ => {
                let oc = self.coll(object, ctx)?;
                self.apply_on_coll(name, &oc, args, ctx)
            }
        }
    }

    /// Functions whose result depends only on the object collection.
    fn apply_on_coll(
        &self,
        name: &str,
        coll: &str,
        args: &[ExpressionNode],
        _ctx: &str,
    ) -> Result<String, Error> {
        match name {
            "first" | "single" => Ok(self.index(coll, 0)),
            "last" => Ok(self.index(coll, -1)),
            "where" => {
                let cond = args
                    .first()
                    .ok_or_else(|| Error::InvalidPath("where() requires an argument".into()))?;
                self.where_fn(coll, cond)
            }
            "exists" => {
                let base = match args.first() {
                    Some(cond) => self.where_fn(coll, cond)?,
                    None => coll.to_string(),
                };
                Ok(self.dialect.build_array1(
                    &self
                        .dialect
                        .to_json_scalar(&format!("{} > 0", self.dialect.array_length(&base))),
                ))
            }
            "empty" => Ok(self.dialect.build_array1(
                &self
                    .dialect
                    .to_json_scalar(&format!("{} = 0", self.dialect.array_length(coll))),
            )),
            "count" => Ok(self.dialect.build_array1(
                &self
                    .dialect
                    .to_json_scalar(&self.dialect.array_length(coll)),
            )),
            "join" => self.join_fn(coll, args),
            "extension" => {
                let url = self.string_text(args.first())?;
                self.extension_fn(coll, &url)
            }
            "getReferenceKey" => self.reference_key(coll, args.first()),
            "getResourceKey" => self.resource_key(coll),
            "lowBoundary" => Ok(self.boundary(coll, true, BoundaryType::Unknown)),
            "highBoundary" => Ok(self.boundary(coll, false, BoundaryType::Unknown)),
            "toString" => Ok(coll.to_string()),
            other => Err(Error::InvalidPath(format!(
                "unsupported function {other}()"
            ))),
        }
    }

    /// `object.where(cond)` — keep the items for which `cond` is true.
    fn where_fn(&self, coll: &str, cond: &ExpressionNode) -> Result<String, Error> {
        let n = self.fresh();
        let item = format!("_w{n}._it");
        let pred = self.bool(cond, &item)?;
        let src = self.dialect.elements_table(coll, &format!("_w{n}"), "_it");
        Ok(format!(
            "(SELECT {} FROM {src} WHERE {pred})",
            self.dialect.agg(&format!("_w{n}._it")),
        ))
    }

    fn join_fn(&self, coll: &str, args: &[ExpressionNode]) -> Result<String, Error> {
        let sep = match args.first() {
            Some(a) => self.string_text(Some(a))?,
            None => String::new(),
        };
        let n = self.fresh();
        let src = self.dialect.elements_table(coll, &format!("_jn{n}"), "_e");
        let joined = self
            .dialect
            .build_array1(&self.dialect.to_json_scalar(&format!(
                "string_agg({}, '{sep}')",
                self.dialect.scalar_text("_e")
            )));
        // join() over an empty collection yields the empty collection (null),
        // not an empty string. (FHIR/sql-on-fhir.js fn_join)
        Ok(format!(
            "(SELECT CASE WHEN count(*) = 0 THEN {empty} ELSE {joined} END FROM {src})",
            empty = self.dialect.empty_array(),
        ))
    }

    fn extension_fn(&self, coll: &str, url: &str) -> Result<String, Error> {
        let nav_ext = self.nav(coll, "extension");
        let n = self.fresh();
        let url = url.replace('\'', "''");
        let src = self
            .dialect
            .elements_table(&nav_ext, &format!("_x{n}"), "_e");
        Ok(format!(
            "(SELECT {agg} FROM {src} WHERE _x{n}._e ->> 'url' = '{url}')",
            agg = self.dialect.agg(&format!("_x{n}._e")),
        ))
    }

    fn reference_key(
        &self,
        coll: &str,
        type_arg: Option<&ExpressionNode>,
    ) -> Result<String, Error> {
        let n = self.fresh();
        let refexpr = format!("_r{n}._e ->> 'reference'");
        let is_frag = format!("{refexpr} LIKE '#%'");
        // Fragment (`#id`) references key on the local id (sans `#`), resolved
        // into the resource's contained[]; relative/absolute references key on
        // the `Type/id` id segment. Both equal getResourceKey() (the id).
        let key = format!(
            "CASE WHEN {is_frag} THEN substring({refexpr} from 2) \
             ELSE split_part({refexpr}, '/', 2) END"
        );
        let guard = match type_arg {
            Some(t) => {
                let ty = self.type_name(t)?.replace('\'', "''");
                // A contained resource's type is its declared `resourceType`,
                // falling back to the Reference.type element; a normal
                // reference's type is its leading path segment.
                let contained_src = self.dialect.elements_table(
                    &format!(
                        "coalesce({root} -> 'contained', {empty})",
                        root = self.root,
                        empty = self.dialect.empty_array()
                    ),
                    "_ct",
                    "_ce",
                );
                let contained_ty = format!(
                    "(SELECT _ct._ce ->> 'resourceType' \
                     FROM {contained_src} \
                     WHERE _ct._ce ->> 'id' = substring({refexpr} from 2) LIMIT 1)"
                );
                format!(
                    " AND (CASE WHEN {is_frag} \
                       THEN coalesce({contained_ty}, _r{n}._e ->> 'type') = '{ty}' \
                       ELSE split_part({refexpr}, '/', 1) = '{ty}' END)"
                )
            }
            None => String::new(),
        };
        let src = self.dialect.elements_table(coll, &format!("_r{n}"), "_e");
        Ok(format!(
            "(SELECT {agg} FROM {src} WHERE {refexpr} IS NOT NULL{guard})",
            agg = self.dialect.agg(&self.dialect.to_json_scalar(&key)),
        ))
    }

    fn resource_key(&self, coll: &str) -> Result<String, Error> {
        let n = self.fresh();
        let src = self.dialect.elements_table(coll, &format!("_k{n}"), "_e");
        Ok(format!(
            "(SELECT {agg} FROM {src})",
            agg = self.dialect.agg(&format!("_k{n}._e -> 'id'")),
        ))
    }

    /// FHIR precision boundary. Numbers use the decimal half-ulp boundary;
    /// date/dateTime/time strings are widened to their precision-filled bounds
    /// (`lowBoundary` to the earliest instant, `highBoundary` to the latest).
    fn boundary(&self, coll: &str, low: bool, hint: BoundaryType) -> String {
        let sign = if low { "-" } else { "+" };
        let n = self.fresh();
        let e = format!("_b{n}._e");
        let t = self.dialect.scalar_text(&e);
        let num = self.dialect.num_cast();
        let numeric = format!(
            "(({t})::{num} {sign} (0.5 / power(10, coalesce(length(nullif(split_part({t}, '.', 2), '')), 0))::{num}))"
        );
        let d = self.dialect;
        let string_bound = match hint {
            BoundaryType::Date => date_bound(d, &t, low),
            BoundaryType::DateTime => datetime_bound(d, &t, low),
            BoundaryType::Time => time_bound(d, &t, low),
            BoundaryType::Unknown => format!(
                "CASE WHEN position(':' in {t}) > 0 THEN {} ELSE {} END",
                time_bound(d, &t, low),
                date_bound(d, &t, low)
            ),
        };
        let src = self.dialect.elements_table(coll, &format!("_b{n}"), "_e");
        format!(
            "(SELECT {agg} FROM {src})",
            agg = self.dialect.agg(&format!(
                "CASE WHEN {is_num} THEN {num_j} WHEN {is_str} THEN {str_j} ELSE {e} END",
                is_num = self.dialect.is_number(&e),
                num_j = self.dialect.to_json_scalar(&numeric),
                is_str = self.dialect.is_string(&e),
                str_j = self.dialect.to_json_scalar(&string_bound),
            )),
        )
    }

    /// `object.ofType(T)` — FHIR choice elements are stored as `<base><Type>`.
    fn of_type(
        &self,
        object: &ExpressionNode,
        type_arg: Option<&ExpressionNode>,
        ctx: &str,
    ) -> Result<String, Error> {
        let arg = type_arg
            .ok_or_else(|| Error::InvalidPath("ofType() requires a type argument".into()))?;
        let tname = capitalize_first(&self.type_name(arg)?);
        match object {
            ExpressionNode::PropertyAccess(p) => {
                let base = self.coll(&p.object, ctx)?;
                Ok(self.nav(&base, &format!("{}{}", p.property, tname)))
            }
            ExpressionNode::Identifier(idn) if idn.name != self.resource_type => Ok(self.nav(
                &self.dialect.build_array1(ctx),
                &format!("{}{}", idn.name, tname),
            )),
            ExpressionNode::Parenthesized(e) => self.of_type(e, type_arg, ctx),
            _ => self.coll(object, ctx),
        }
    }

    // --- Boolean lowering for `where` filters and comparisons. ---

    pub(super) fn bool(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        match node {
            ExpressionNode::Parenthesized(e) => self.bool(e, item),
            ExpressionNode::UnaryOperation(u) if matches!(u.operator, UnaryOperator::Not) => {
                Ok(format!("(NOT {})", self.bool(&u.operand, item)?))
            }
            ExpressionNode::BinaryOperation(b) => self.bool_binop(b, item),
            ExpressionNode::Literal(l) => match &l.value {
                LiteralValue::Boolean(v) => Ok(v.to_string()),
                _ => self.scalar_bool(node, item),
            },
            _ => self.scalar_bool(node, item),
        }
    }

    /// Extract the singleton of `node` as text and cast to a SQL boolean.
    fn scalar_bool(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        Ok(format!(
            "{}::{}",
            self.dialect.scalar_text(&self.scalar_jsonb(node, item)?),
            self.dialect.bool_cast()
        ))
    }

    fn bool_binop(
        &self,
        b: &octofhir_fhirpath::ast::BinaryOperationNode,
        item: &str,
    ) -> Result<String, Error> {
        use BinaryOperator::*;
        let logical = |op: &str, l: &str, r: &str| format!("({l} {op} {r})");
        match b.operator {
            And => Ok(logical(
                "AND",
                &self.bool(&b.left, item)?,
                &self.bool(&b.right, item)?,
            )),
            Or => Ok(logical(
                "OR",
                &self.bool(&b.left, item)?,
                &self.bool(&b.right, item)?,
            )),
            Xor => Ok(logical(
                "<>",
                &self.bool(&b.left, item)?,
                &self.bool(&b.right, item)?,
            )),
            Implies => Ok(format!(
                "((NOT {}) OR {})",
                self.bool(&b.left, item)?,
                self.bool(&b.right, item)?
            )),
            Equal | Equivalent => Ok(format!(
                "({} = {})",
                self.scalar_jsonb(&b.left, item)?,
                self.scalar_jsonb(&b.right, item)?
            )),
            NotEqual | NotEquivalent => Ok(format!(
                "({} <> {})",
                self.scalar_jsonb(&b.left, item)?,
                self.scalar_jsonb(&b.right, item)?
            )),
            LessThan => self.cmp("<", b, item),
            LessThanOrEqual => self.cmp("<=", b, item),
            GreaterThan => self.cmp(">", b, item),
            GreaterThanOrEqual => self.cmp(">=", b, item),
            other => Err(Error::InvalidPath(format!(
                "unsupported boolean operator {other:?}"
            ))),
        }
    }

    fn cmp(
        &self,
        op: &str,
        b: &octofhir_fhirpath::ast::BinaryOperationNode,
        item: &str,
    ) -> Result<String, Error> {
        Ok(format!(
            "({} {op} {})",
            self.scalar_jsonb(&b.left, item)?,
            self.scalar_jsonb(&b.right, item)?
        ))
    }

    /// A single JSONB value (the FHIRPath singleton) for use in comparisons.
    fn scalar_jsonb(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        if let ExpressionNode::Literal(l) = node {
            return self.literal_jsonb(&l.value);
        }
        let coll = self.coll(node, item)?;
        let n = self.fresh();
        let src = self.dialect.elements_table(&coll, &format!("_sq{n}"), "_s");
        Ok(format!("(SELECT _s FROM {src})"))
    }

    /// A JSONB value for a binary/unary expression used as a column value.
    fn value_jsonb(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        match node {
            ExpressionNode::BinaryOperation(b) => {
                use BinaryOperator::*;
                let num = self.dialect.num_cast();
                let lnum = |g: &Self| -> Result<String, Error> {
                    Ok(format!(
                        "{}::{num}",
                        g.dialect.scalar_text(&g.scalar_jsonb(&b.left, item)?)
                    ))
                };
                let rnum = |g: &Self| -> Result<String, Error> {
                    Ok(format!(
                        "{}::{num}",
                        g.dialect.scalar_text(&g.scalar_jsonb(&b.right, item)?)
                    ))
                };
                let arith = |g: &Self, op: &str| -> Result<String, Error> {
                    Ok(g.dialect
                        .to_json_scalar(&format!("{} {op} {}", lnum(g)?, rnum(g)?)))
                };
                match b.operator {
                    Add => arith(self, "+"),
                    Subtract => arith(self, "-"),
                    Multiply => arith(self, "*"),
                    Divide => arith(self, "/"),
                    Modulo => arith(self, "%"),
                    IntegerDivide => Ok(self.dialect.to_json_scalar(&format!(
                        "trunc({} / {})",
                        lnum(self)?,
                        rnum(self)?
                    ))),
                    Concatenate => Ok(self.dialect.to_json_scalar(&format!(
                        "{} || {}",
                        self.dialect.scalar_text(&self.scalar_jsonb(&b.left, item)?),
                        self.dialect
                            .scalar_text(&self.scalar_jsonb(&b.right, item)?)
                    ))),
                    _ => Ok(self.dialect.to_json_scalar(&self.bool(node, item)?)),
                }
            }
            ExpressionNode::UnaryOperation(u) => match u.operator {
                UnaryOperator::Not => Ok(self.dialect.to_json_scalar(&self.bool(node, item)?)),
                UnaryOperator::Negate => Ok(self.dialect.to_json_scalar(&format!(
                    "- {}::{}",
                    self.dialect
                        .scalar_text(&self.scalar_jsonb(&u.operand, item)?),
                    self.dialect.num_cast()
                ))),
                UnaryOperator::Positive => Ok(self.dialect.to_json_scalar(&format!(
                    "{}::{}",
                    self.dialect
                        .scalar_text(&self.scalar_jsonb(&u.operand, item)?),
                    self.dialect.num_cast()
                ))),
            },
            _ => Ok(self.dialect.to_json_scalar(&self.bool(node, item)?)),
        }
    }

    fn literal_jsonb(&self, v: &LiteralValue) -> Result<String, Error> {
        let txt = self.dialect.text_cast();
        let num = self.dialect.num_cast();
        let int = self.dialect.int_cast();
        let j = |x: &str| self.dialect.to_json_scalar(x);
        Ok(match v {
            LiteralValue::String(s) => j(&format!("'{}'::{txt}", s.replace('\'', "''"))),
            LiteralValue::Integer(i) | LiteralValue::Long(i) => j(&format!("{i}::{int}")),
            LiteralValue::Decimal(d) => j(&format!("{d}::{num}")),
            LiteralValue::Boolean(b) => j(&b.to_string()),
            LiteralValue::Date(_) | LiteralValue::DateTime(_) | LiteralValue::Time(_) => {
                let s = v.to_string();
                let s = s.trim_start_matches('@');
                j(&format!("'{}'::{txt}", s.replace('\'', "''")))
            }
            LiteralValue::Quantity { value, .. } => j(&format!("{value}::{num}")),
        })
    }

    fn int_literal(&self, node: &ExpressionNode) -> Result<i64, Error> {
        match node {
            ExpressionNode::Literal(l) => match &l.value {
                LiteralValue::Integer(i) | LiteralValue::Long(i) => Ok(*i),
                other => Err(Error::InvalidPath(format!(
                    "expected integer index, got {other}"
                ))),
            },
            ExpressionNode::Parenthesized(e) => self.int_literal(e),
            other => Err(Error::InvalidPath(format!(
                "index must be an integer literal, got {}",
                other.node_type()
            ))),
        }
    }

    fn type_name(&self, node: &ExpressionNode) -> Result<String, Error> {
        match node {
            ExpressionNode::Identifier(n) => Ok(n.name.clone()),
            ExpressionNode::TypeInfo(t) => Ok(t.name.clone()),
            other => Err(Error::InvalidPath(format!(
                "expected a type name, got {}",
                other.node_type()
            ))),
        }
    }

    fn string_text(&self, node: Option<&ExpressionNode>) -> Result<String, Error> {
        match node {
            Some(ExpressionNode::Literal(l)) => match &l.value {
                LiteralValue::String(s) => Ok(s.clone()),
                other => Ok(other.to_string()),
            },
            Some(other) => Err(Error::InvalidPath(format!(
                "expected a string literal, got {}",
                other.node_type()
            ))),
            None => Ok(String::new()),
        }
    }

    /// Replace `%name` constant references with their FHIRPath literal text.
    /// Errors on a reference to an undefined constant.
    pub(super) fn substitute(&self, path: &str) -> Result<String, Error> {
        substitute_constants(path, &self.constants)
    }
}
