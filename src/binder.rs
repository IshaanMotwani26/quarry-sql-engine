//! Binder: AST -> `BoundQuery`.
//!
//! Responsibilities:
//! - Resolve every column reference to a `ColumnId`, searching enclosing
//!   queries for correlated references and reporting unknown or ambiguous names.
//! - Type-check every expression, inserting implicit casts (BIGINT -> DOUBLE,
//!   NULL -> anything) and coercing string literals compared against dates.
//! - Validate aggregation: aggregates aren't allowed in WHERE or GROUP BY, can't
//!   nest, and in an aggregating query every output must be a group key, an
//!   aggregate, or built from them.
//! - Resolve ORDER BY against output aliases, positions, or extra hidden columns.

use std::collections::HashSet;
use std::fmt;

use crate::ast::{
    BinaryOp, DateTimeField, Expr, FunctionArgs, JoinKind, Literal, Query, SelectItem, TableAlias,
    TableFactor, TableWithJoins, UnaryOp,
};
use crate::bound::*;
use crate::catalog::Catalog;
use crate::types::{parse_date, Type, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindError(pub String);

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BindError {}

type Result<T> = std::result::Result<T, BindError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(BindError(msg.into()))
}

/// Binds a parsed query against `catalog`.
pub fn bind(catalog: &Catalog, query: &Query) -> Result<Bound> {
    let mut binder = Binder {
        catalog,
        columns: Vec::new(),
        scopes: Vec::new(),
    };
    let query = binder.bind_query(query)?;
    Ok(Bound {
        query,
        columns: binder.columns,
    })
}

/// One FROM item as seen by name resolution.
struct Relation {
    qualifier: Option<String>,
    columns: Vec<(String, ColumnId)>,
}

/// Per-query binding state. Subqueries push a new scope.
struct Scope {
    relations: Vec<Relation>,
    /// False while binding a derived table: sibling FROM items aren't
    /// visible to it (there is no LATERAL).
    visible_to_children: bool,
    correlated: Vec<ColumnId>,
    aggregates: Vec<AggregateCall>,
    clause: &'static str,
    agg_allowed: bool,
    in_aggregate_arg: bool,
}

impl Scope {
    fn new() -> Self {
        Scope {
            relations: Vec::new(),
            visible_to_children: true,
            correlated: Vec::new(),
            aggregates: Vec::new(),
            clause: "",
            agg_allowed: false,
            in_aggregate_arg: false,
        }
    }
}

struct Binder<'a> {
    catalog: &'a Catalog,
    columns: Vec<ColumnInfo>,
    scopes: Vec<Scope>,
}

impl Binder<'_> {
    fn scope(&mut self) -> &mut Scope {
        self.scopes
            .last_mut()
            .expect("binder always has an active scope")
    }

    fn enter_clause(&mut self, clause: &'static str, agg_allowed: bool) {
        let s = self.scope();
        s.clause = clause;
        s.agg_allowed = agg_allowed;
    }

    fn new_column(
        &mut self,
        qualifier: Option<String>,
        name: impl Into<String>,
        ty: Type,
    ) -> ColumnId {
        let id = self.columns.len();
        self.columns.push(ColumnInfo {
            id,
            qualifier,
            name: name.into(),
            ty,
        });
        id
    }

    fn local_columns(&self) -> HashSet<ColumnId> {
        let scope = self.scopes.last().expect("active scope");
        scope
            .relations
            .iter()
            .flat_map(|r| r.columns.iter().map(|(_, id)| *id))
            .collect()
    }

    // ---- queries ---------------------------------------------------------

    fn bind_query(&mut self, q: &Query) -> Result<BoundQuery> {
        self.scopes.push(Scope::new());
        let result = self.bind_select(q);
        let scope = self.scopes.pop().expect("pushed above");
        let mut bound = result?;
        bound.correlated = scope.correlated;
        Ok(bound)
    }

    fn bind_select(&mut self, q: &Query) -> Result<BoundQuery> {
        // FROM: comma-separated items become a left-deep chain of cross joins.
        self.enter_clause("FROM", false);
        let mut from: Option<BoundTableRef> = None;
        for item in &q.from {
            let right = self.bind_table_with_joins(item)?;
            from = Some(match from.take() {
                None => right,
                Some(left) => BoundTableRef::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind: JoinKind::Cross,
                    on: None,
                },
            });
        }

        let filter = match &q.selection {
            Some(e) => Some(self.bind_condition(e, "WHERE", false)?),
            None => None,
        };

        self.enter_clause("GROUP BY", false);
        let mut group_by: Vec<GroupKey> = Vec::new();
        for e in &q.group_by {
            let expr = self.bind_expr(e)?;
            if !group_by.iter().any(|k| k.expr == expr) {
                let id = self.new_column(None, e.to_string(), expr.ty);
                group_by.push(GroupKey { id, expr });
            }
        }

        self.enter_clause("SELECT", true);
        let mut items: Vec<(String, BoundExpr)> = Vec::new();
        for item in &q.projection {
            self.bind_select_item(item, &mut items)?;
        }
        let visible = items.len();

        let having = match &q.having {
            Some(e) => Some(self.bind_condition(e, "HAVING", true)?),
            None => None,
        };

        self.enter_clause("ORDER BY", true);
        let mut order = Vec::new();
        for ob in &q.order_by {
            let idx = self.bind_order_target(&ob.expr, &mut items, visible, q.distinct)?;
            // Postgres default: NULLs sort as if larger than every value.
            order.push((idx, ob.asc, ob.nulls_first.unwrap_or(!ob.asc)));
        }

        let limit = q
            .limit
            .as_ref()
            .map(|e| bind_count(e, "LIMIT"))
            .transpose()?;
        let offset = q
            .offset
            .as_ref()
            .map(|e| bind_count(e, "OFFSET"))
            .transpose()?;

        // Above the aggregate, only group keys and aggregate results exist.
        let aggregates = std::mem::take(&mut self.scope().aggregates);
        let is_aggregate = !group_by.is_empty() || !aggregates.is_empty() || having.is_some();
        let (items, having) = if is_aggregate {
            let local = self.local_columns();
            let rewrite = |e: BoundExpr| rewrite_grouped(e, &group_by, &local, &self.columns);
            let items = items
                .into_iter()
                .map(|(name, e)| Ok((name, rewrite(e)?)))
                .collect::<Result<Vec<_>>>()?;
            (items, having.map(rewrite).transpose()?)
        } else {
            (items, having)
        };

        let projection: Vec<OutputColumn> = items
            .into_iter()
            .map(|(name, expr)| {
                let id = self.new_column(None, name.clone(), expr.ty);
                OutputColumn { id, name, expr }
            })
            .collect();
        let order_by = order
            .into_iter()
            .map(|(i, asc, nulls_first)| BoundOrderBy {
                column: projection[i].id,
                asc,
                nulls_first,
            })
            .collect();

        Ok(BoundQuery {
            from,
            filter,
            group_by,
            aggregates,
            having,
            projection,
            visible,
            distinct: q.distinct,
            order_by,
            limit,
            offset,
            correlated: Vec::new(),
        })
    }

    fn bind_condition(
        &mut self,
        e: &Expr,
        clause: &'static str,
        agg_allowed: bool,
    ) -> Result<BoundExpr> {
        self.enter_clause(clause, agg_allowed);
        let bound = self.bind_expr(e)?;
        coerce(bound, Type::Boolean, &format!("argument of {clause}"))
    }

    fn bind_select_item(
        &mut self,
        item: &SelectItem,
        items: &mut Vec<(String, BoundExpr)>,
    ) -> Result<()> {
        match item {
            SelectItem::Wildcard => {
                let cols = self.relation_columns(None)?;
                if cols.is_empty() {
                    return err("SELECT * with no tables specified is not valid");
                }
                items.extend(cols);
            }
            SelectItem::QualifiedWildcard(t) => items.extend(self.relation_columns(Some(t))?),
            SelectItem::Expr { expr, alias } => {
                let bound = self.bind_expr(expr)?;
                let name = alias.clone().unwrap_or_else(|| output_name(expr));
                items.push((name, bound));
            }
        }
        Ok(())
    }

    fn relation_columns(&self, qualifier: Option<&str>) -> Result<Vec<(String, BoundExpr)>> {
        let scope = self.scopes.last().expect("active scope");
        let mut out = Vec::new();
        let mut matched = false;
        for rel in &scope.relations {
            if qualifier.is_some() && rel.qualifier.as_deref() != qualifier {
                continue;
            }
            matched = true;
            for (name, id) in &rel.columns {
                out.push((name.clone(), BoundExpr::column(*id, self.columns[*id].ty)));
            }
        }
        match qualifier {
            Some(q) if !matched => err(format!("missing FROM-clause entry for table \"{q}\"")),
            _ => Ok(out),
        }
    }

    /// Returns the index into `items` that an ORDER BY key sorts on, adding a
    /// hidden item if the key isn't already computed.
    fn bind_order_target(
        &mut self,
        e: &Expr,
        items: &mut Vec<(String, BoundExpr)>,
        visible: usize,
        distinct: bool,
    ) -> Result<usize> {
        if let Expr::Literal(Literal::Number(n)) = e {
            return match n.parse::<usize>() {
                Ok(k) if (1..=visible).contains(&k) => Ok(k - 1),
                _ => err(format!("ORDER BY position {n} is not in select list")),
            };
        }
        if let Expr::Column { table: None, name } = e {
            let matches: Vec<usize> = (0..visible).filter(|&i| items[i].0 == *name).collect();
            if let Some(&first) = matches.first() {
                if matches.iter().any(|&i| items[i].1 != items[first].1) {
                    return err(format!("ORDER BY \"{name}\" is ambiguous"));
                }
                return Ok(first);
            }
        }
        let bound = self.bind_expr(e)?;
        if let Some(i) = items.iter().position(|(_, x)| *x == bound) {
            return Ok(i);
        }
        if distinct {
            return err("for SELECT DISTINCT, ORDER BY expressions must appear in select list");
        }
        items.push((e.to_string(), bound));
        Ok(items.len() - 1)
    }

    // ---- FROM clause -----------------------------------------------------

    fn bind_table_with_joins(&mut self, t: &TableWithJoins) -> Result<BoundTableRef> {
        let mut left = self.bind_table_factor(&t.relation)?;
        for j in &t.joins {
            let right = self.bind_table_factor(&j.relation)?;
            let on = match &j.on {
                Some(e) => Some(self.bind_condition(e, "JOIN/ON", false)?),
                None => None,
            };
            left = BoundTableRef::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind: j.kind,
                on,
            };
        }
        Ok(left)
    }

    fn bind_table_factor(&mut self, factor: &TableFactor) -> Result<BoundTableRef> {
        match factor {
            TableFactor::Table { name, alias } => {
                let catalog = self.catalog;
                let table = catalog
                    .get(name)
                    .ok_or_else(|| BindError(format!("table \"{name}\" does not exist")))?;
                let fields = table
                    .schema
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty))
                    .collect();
                let fields = apply_column_aliases(alias.as_ref(), name, fields)?;
                let qualifier = alias
                    .as_ref()
                    .map_or_else(|| name.clone(), |a| a.name.clone());
                let columns = self.add_relation(Some(qualifier), fields)?;
                Ok(BoundTableRef::Base {
                    table: name.clone(),
                    columns,
                })
            }
            TableFactor::Derived { subquery, alias } => {
                let parent = self.scopes.len() - 1;
                self.scopes[parent].visible_to_children = false;
                let result = self.bind_query(subquery);
                self.scopes[parent].visible_to_children = true;
                let query = result?;

                let fields = query
                    .outputs()
                    .iter()
                    .map(|o| (o.name.clone(), o.ty()))
                    .collect();
                let label = alias.as_ref().map_or("subquery", |a| a.name.as_str());
                let fields = apply_column_aliases(alias.as_ref(), label, fields)?;
                let columns = self.add_relation(alias.as_ref().map(|a| a.name.clone()), fields)?;
                Ok(BoundTableRef::Derived {
                    query: Box::new(query),
                    columns,
                })
            }
        }
    }

    fn add_relation(
        &mut self,
        qualifier: Option<String>,
        fields: Vec<(String, Type)>,
    ) -> Result<Vec<ColumnId>> {
        if let Some(q) = &qualifier {
            let scope = self.scopes.last().expect("active scope");
            if scope
                .relations
                .iter()
                .any(|r| r.qualifier.as_ref() == Some(q))
            {
                return err(format!("table name \"{q}\" specified more than once"));
            }
        }
        let mut columns = Vec::with_capacity(fields.len());
        for (name, ty) in fields {
            let id = self.new_column(qualifier.clone(), name.clone(), ty);
            columns.push((name, id));
        }
        let ids = columns.iter().map(|(_, id)| *id).collect();
        self.scope().relations.push(Relation { qualifier, columns });
        Ok(ids)
    }

    // ---- name resolution -------------------------------------------------

    fn resolve_column(&mut self, table: Option<&str>, name: &str) -> Result<BoundExpr> {
        let innermost = self.scopes.len() - 1;
        for level in (0..=innermost).rev() {
            let scope = &self.scopes[level];
            if level < innermost && !scope.visible_to_children {
                continue;
            }
            let mut found = None;
            let mut qualifier_matched = false;
            for rel in &scope.relations {
                if table.is_some() {
                    if rel.qualifier.as_deref() != table {
                        continue;
                    }
                    qualifier_matched = true;
                }
                for (col, id) in &rel.columns {
                    if col == name {
                        if found.is_some() {
                            return err(format!("column reference \"{name}\" is ambiguous"));
                        }
                        found = Some(*id);
                    }
                }
            }
            if let Some(id) = found {
                // Every query between the reference and the definition is correlated.
                for outer in &mut self.scopes[level + 1..] {
                    if !outer.correlated.contains(&id) {
                        outer.correlated.push(id);
                    }
                }
                return Ok(BoundExpr::column(id, self.columns[id].ty));
            }
            if qualifier_matched {
                return err(format!(
                    "column {}.{name} does not exist",
                    table.unwrap_or_default()
                ));
            }
        }
        match table {
            Some(t) => err(format!("missing FROM-clause entry for table \"{t}\"")),
            None => err(format!("column \"{name}\" does not exist")),
        }
    }

    // ---- expressions -----------------------------------------------------

    fn bind_expr(&mut self, e: &Expr) -> Result<BoundExpr> {
        match e {
            Expr::Column { table, name } => self.resolve_column(table.as_deref(), name),
            Expr::Literal(l) => bind_literal(l),
            Expr::Unary { op, expr } => {
                let inner = self.bind_expr(expr)?;
                bind_unary(*op, inner)
            }
            Expr::Binary { left, op, right } => {
                let l = self.bind_expr(left)?;
                let r = self.bind_expr(right)?;
                bind_binary(*op, l, r)
            }
            Expr::IsNull { expr, negated } => {
                let inner = self.bind_expr(expr)?;
                Ok(BoundExpr::new(
                    ExprKind::IsNull {
                        expr: Box::new(inner),
                        negated: *negated,
                    },
                    Type::Boolean,
                ))
            }
            Expr::Like {
                expr,
                pattern,
                negated,
            } => {
                let x = coerce(self.bind_expr(expr)?, Type::Utf8, "argument of LIKE")?;
                let p = coerce(self.bind_expr(pattern)?, Type::Utf8, "LIKE pattern")?;
                let kind = ExprKind::Like {
                    expr: Box::new(x),
                    pattern: Box::new(p),
                    negated: *negated,
                };
                Ok(BoundExpr::new(kind, Type::Boolean))
            }
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                // x BETWEEN a AND b  ==>  x >= a AND x <= b
                let x = self.bind_expr(expr)?;
                let lo = self.bind_expr(low)?;
                let hi = self.bind_expr(high)?;
                let ge = bind_comparison(BinaryOp::GtEq, x.clone(), lo)?;
                let le = bind_comparison(BinaryOp::LtEq, x, hi)?;
                let both = binary(ge, BinaryOp::And, le, Type::Boolean);
                Ok(if *negated { not(both) } else { both })
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let x = self.bind_expr(expr)?;
                let mut items = Vec::with_capacity(list.len());
                for item in list {
                    let bound = self.bind_expr(item)?;
                    items.push(adapt_literal(x.ty, bound)?);
                }
                let mut target = x.ty;
                for it in &items {
                    target = Type::common(target, it.ty).ok_or_else(|| {
                        BindError(format!("IN types {target} and {} cannot be matched", it.ty))
                    })?;
                }
                let list = items.into_iter().map(|i| cast(i, target)).collect();
                let kind = ExprKind::InList {
                    expr: Box::new(cast(x, target)),
                    list,
                    negated: *negated,
                };
                Ok(BoundExpr::new(kind, Type::Boolean))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let x = self.bind_expr(expr)?;
                let query = self.bind_query(subquery)?;
                let sub_ty = single_column_type(&query)?;
                let target = Type::common(x.ty, sub_ty)
                    .filter(|t| *t == sub_ty || sub_ty == Type::Null)
                    .ok_or_else(|| {
                        BindError(format!(
                            "operator does not exist: {} IN (subquery of {sub_ty})",
                            x.ty
                        ))
                    })?;
                let kind = SubqueryKind::In {
                    expr: Box::new(cast(x, target)),
                    negated: *negated,
                };
                Ok(BoundExpr::new(
                    ExprKind::Subquery {
                        query: Box::new(query),
                        kind,
                    },
                    Type::Boolean,
                ))
            }
            Expr::Exists { subquery, negated } => {
                let query = self.bind_query(subquery)?;
                let kind = SubqueryKind::Exists { negated: *negated };
                Ok(BoundExpr::new(
                    ExprKind::Subquery {
                        query: Box::new(query),
                        kind,
                    },
                    Type::Boolean,
                ))
            }
            Expr::Subquery(q) => {
                let query = self.bind_query(q)?;
                let ty = single_column_type(&query)?;
                Ok(BoundExpr::new(
                    ExprKind::Subquery {
                        query: Box::new(query),
                        kind: SubqueryKind::Scalar,
                    },
                    ty,
                ))
            }
            Expr::Function {
                name,
                args,
                distinct,
            } => self.bind_function(e, name, args, *distinct),
            Expr::Case {
                operand,
                branches,
                else_result,
            } => self.bind_case(operand.as_deref(), branches, else_result.as_deref()),
            Expr::Cast { expr, data_type } => {
                let inner = self.bind_expr(expr)?;
                let to = Type::from_ast(data_type);
                if !castable(inner.ty, to) {
                    return err(format!("cannot cast type {} to {to}", inner.ty));
                }
                Ok(cast(inner, to))
            }
            Expr::Extract { field, expr } => {
                let inner = coerce(self.bind_expr(expr)?, Type::Date, "argument of EXTRACT")?;
                let func = match field {
                    DateTimeField::Year => ScalarFunc::ExtractYear,
                    DateTimeField::Month => ScalarFunc::ExtractMonth,
                    DateTimeField::Day => ScalarFunc::ExtractDay,
                    other => return err(format!("EXTRACT({other}) is not supported for DATE")),
                };
                Ok(BoundExpr::new(
                    ExprKind::Function {
                        func,
                        args: vec![inner],
                    },
                    Type::Int64,
                ))
            }
        }
    }

    fn bind_function(
        &mut self,
        whole: &Expr,
        name: &str,
        args: &FunctionArgs,
        distinct: bool,
    ) -> Result<BoundExpr> {
        let agg = match name {
            "count" => Some(AggFunc::Count),
            "sum" => Some(AggFunc::Sum),
            "avg" => Some(AggFunc::Avg),
            "min" => Some(AggFunc::Min),
            "max" => Some(AggFunc::Max),
            _ => None,
        };
        if let Some(func) = agg {
            return self.bind_aggregate(whole, func, name, args, distinct);
        }
        if distinct {
            return err(format!(
                "DISTINCT specified, but {name} is not an aggregate function"
            ));
        }
        let list = match args {
            FunctionArgs::Star => return err(format!("{name}(*) is not valid")),
            FunctionArgs::List(l) => l,
        };
        let mut bound = Vec::with_capacity(list.len());
        for a in list {
            bound.push(self.bind_expr(a)?);
        }
        let n = bound.len();
        let mut it = bound.into_iter();
        let (func, ty, args) = match name {
            "substring" | "substr" => {
                check_arity(name, n, 2, 3)?;
                let mut args = vec![coerce(it.next().unwrap(), Type::Utf8, "substring string")?];
                for a in it {
                    args.push(coerce(a, Type::Int64, "substring position")?);
                }
                (ScalarFunc::Substring, Type::Utf8, args)
            }
            "upper" | "lower" => {
                check_arity(name, n, 1, 1)?;
                let func = if name == "upper" {
                    ScalarFunc::Upper
                } else {
                    ScalarFunc::Lower
                };
                (
                    func,
                    Type::Utf8,
                    vec![coerce(
                        it.next().unwrap(),
                        Type::Utf8,
                        &format!("argument of {name}"),
                    )?],
                )
            }
            "length" | "char_length" => {
                check_arity(name, n, 1, 1)?;
                let arg = coerce(it.next().unwrap(), Type::Utf8, "argument of length")?;
                (ScalarFunc::Length, Type::Int64, vec![arg])
            }
            "abs" => {
                check_arity(name, n, 1, 1)?;
                let a = it.next().unwrap();
                if !(a.ty.is_numeric() || a.ty == Type::Null) {
                    return err(format!("function abs({}) does not exist", a.ty));
                }
                (ScalarFunc::Abs, a.ty, vec![a])
            }
            "round" => {
                check_arity(name, n, 1, 2)?;
                let mut args = vec![coerce(
                    it.next().unwrap(),
                    Type::Float64,
                    "argument of round",
                )?];
                if let Some(digits) = it.next() {
                    args.push(coerce(digits, Type::Int64, "round precision")?);
                }
                (ScalarFunc::Round, Type::Float64, args)
            }
            "coalesce" => {
                check_arity(name, n, 1, usize::MAX)?;
                let args: Vec<BoundExpr> = it.collect();
                let mut target = Type::Null;
                for a in &args {
                    target = Type::common(target, a.ty).ok_or_else(|| {
                        BindError(format!(
                            "COALESCE types {target} and {} cannot be matched",
                            a.ty
                        ))
                    })?;
                }
                (
                    ScalarFunc::Coalesce,
                    target,
                    args.into_iter().map(|a| cast(a, target)).collect(),
                )
            }
            _ => return err(format!("function {name} does not exist")),
        };
        Ok(BoundExpr::new(ExprKind::Function { func, args }, ty))
    }

    fn bind_aggregate(
        &mut self,
        whole: &Expr,
        func: AggFunc,
        name: &str,
        args: &FunctionArgs,
        distinct: bool,
    ) -> Result<BoundExpr> {
        {
            let scope = self.scopes.last().expect("active scope");
            if !scope.agg_allowed {
                return err(format!(
                    "aggregate functions are not allowed in {}",
                    scope.clause
                ));
            }
            if scope.in_aggregate_arg {
                return err("aggregate function calls cannot be nested");
            }
        }
        let arg = match args {
            FunctionArgs::Star => {
                if func != AggFunc::Count || distinct {
                    return err(format!("{name}(*) is not valid"));
                }
                None
            }
            FunctionArgs::List(list) => {
                if list.len() != 1 {
                    return err(format!("{name} takes exactly one argument"));
                }
                self.scope().in_aggregate_arg = true;
                let result = self.bind_expr(&list[0]);
                self.scope().in_aggregate_arg = false;
                Some(result?)
            }
        };

        let numeric = |a: &BoundExpr| a.ty.is_numeric() || a.ty == Type::Null;
        let (func, arg, ty) = match (func, arg) {
            (AggFunc::Count, None) => (AggFunc::CountStar, None, Type::Int64),
            (AggFunc::Count, Some(a)) => (AggFunc::Count, Some(a), Type::Int64),
            (AggFunc::Sum, Some(a)) if a.ty == Type::Float64 => {
                (AggFunc::Sum, Some(a), Type::Float64)
            }
            (AggFunc::Sum, Some(a)) if numeric(&a) => {
                (AggFunc::Sum, Some(cast(a, Type::Int64)), Type::Int64)
            }
            (AggFunc::Avg, Some(a)) if numeric(&a) => {
                (AggFunc::Avg, Some(cast(a, Type::Float64)), Type::Float64)
            }
            (AggFunc::Min | AggFunc::Max, Some(a)) if a.ty != Type::Interval => {
                let ty = a.ty;
                (func, Some(a), ty)
            }
            (_, Some(a)) => return err(format!("function {name}({}) does not exist", a.ty)),
            (_, None) => unreachable!("only count accepts *"),
        };

        let existing = self
            .scopes
            .last()
            .expect("active scope")
            .aggregates
            .iter()
            .find(|a| a.func == func && a.arg == arg && a.distinct == distinct);
        if let Some(a) = existing {
            return Ok(BoundExpr::column(a.id, a.ty));
        }
        let id = self.new_column(None, whole.to_string(), ty);
        self.scope().aggregates.push(AggregateCall {
            id,
            func,
            arg,
            distinct,
            ty,
        });
        Ok(BoundExpr::column(id, ty))
    }

    fn bind_case(
        &mut self,
        operand: Option<&Expr>,
        branches: &[(Expr, Expr)],
        else_result: Option<&Expr>,
    ) -> Result<BoundExpr> {
        let operand = operand.map(|o| self.bind_expr(o)).transpose()?;
        let mut whens = Vec::with_capacity(branches.len());
        let mut thens = Vec::with_capacity(branches.len());
        for (w, t) in branches {
            whens.push(self.bind_expr(w)?);
            thens.push(self.bind_expr(t)?);
        }
        let else_result = else_result.map(|e| self.bind_expr(e)).transpose()?;

        let (operand, whens): (Option<Box<BoundExpr>>, Vec<BoundExpr>) = match operand {
            // CASE x WHEN a THEN ...: x and every WHEN value must share a type.
            Some(op) => {
                let op_ty = op.ty;
                let whens = whens
                    .into_iter()
                    .map(|w| adapt_literal(op_ty, w))
                    .collect::<Result<Vec<_>>>()?;
                let mut target = op_ty;
                for w in &whens {
                    target = Type::common(target, w.ty).ok_or_else(|| {
                        BindError(format!(
                            "CASE types {target} and {} cannot be matched",
                            w.ty
                        ))
                    })?;
                }
                (
                    Some(Box::new(cast(op, target))),
                    whens.into_iter().map(|w| cast(w, target)).collect(),
                )
            }
            None => {
                let whens = whens
                    .into_iter()
                    .map(|w| coerce(w, Type::Boolean, "argument of CASE/WHEN"))
                    .collect::<Result<_>>()?;
                (None, whens)
            }
        };

        let mut ty = Type::Null;
        for t in thens.iter().chain(else_result.iter()) {
            ty = Type::common(ty, t.ty).ok_or_else(|| {
                BindError(format!("CASE types {ty} and {} cannot be matched", t.ty))
            })?;
        }
        let branches = whens
            .into_iter()
            .zip(thens.into_iter().map(|t| cast(t, ty)))
            .collect();
        let else_result = else_result.map(|e| Box::new(cast(e, ty)));
        Ok(BoundExpr::new(
            ExprKind::Case {
                operand,
                branches,
                else_result,
            },
            ty,
        ))
    }
}

// ---------------------------------------------------------------------------
// Free helpers (no binder state needed)
// ---------------------------------------------------------------------------

fn apply_column_aliases(
    alias: Option<&TableAlias>,
    label: &str,
    mut fields: Vec<(String, Type)>,
) -> Result<Vec<(String, Type)>> {
    if let Some(a) = alias {
        if a.columns.len() > fields.len() {
            return err(format!(
                "table \"{label}\" has {} columns available but {} columns specified",
                fields.len(),
                a.columns.len()
            ));
        }
        for (field, new_name) in fields.iter_mut().zip(&a.columns) {
            field.0 = new_name.clone();
        }
    }
    Ok(fields)
}

/// In an aggregating query, replaces group-key expressions with references to
/// the key and rejects any remaining reference to a raw table column.
fn rewrite_grouped(
    e: BoundExpr,
    keys: &[GroupKey],
    local: &HashSet<ColumnId>,
    columns: &[ColumnInfo],
) -> Result<BoundExpr> {
    if let Some(k) = keys.iter().find(|k| k.expr == e) {
        return Ok(BoundExpr::column(k.id, e.ty));
    }
    if let ExprKind::Column(id) = &e.kind {
        if local.contains(id) {
            return err(format!(
                "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                columns[*id].display_name()
            ));
        }
    }
    e.try_map_children(&mut |c| rewrite_grouped(c, keys, local, columns))
}

fn single_column_type(q: &BoundQuery) -> Result<Type> {
    match q.outputs() {
        [only] => Ok(only.ty()),
        _ => err("subquery must return only one column"),
    }
}

fn check_arity(name: &str, n: usize, min: usize, max: usize) -> Result<()> {
    if (min..=max).contains(&n) {
        return Ok(());
    }
    let expected = match (min, max) {
        (a, b) if a == b => format!("{a}"),
        (a, usize::MAX) => format!("at least {a}"),
        (a, b) => format!("{a} to {b}"),
    };
    err(format!(
        "function {name} expects {expected} argument(s), got {n}"
    ))
}

fn output_name(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::Function { name, .. } => name.clone(),
        Expr::Cast { expr, .. } => output_name(expr),
        Expr::Extract { .. } => "extract".into(),
        Expr::Case { .. } => "case".into(),
        Expr::Exists { .. } => "exists".into(),
        _ => "?column?".into(),
    }
}

fn bind_count(e: &Expr, clause: &str) -> Result<u64> {
    match e {
        Expr::Literal(Literal::Number(n)) => n
            .parse()
            .map_err(|_| BindError(format!("{clause} must be a non-negative integer, got {n}"))),
        other => err(format!(
            "{clause} must be a non-negative integer constant, got {other}"
        )),
    }
}

fn bind_literal(l: &Literal) -> Result<BoundExpr> {
    let v = match l {
        Literal::Number(n) => {
            let float = || {
                n.parse()
                    .map(Value::Float64)
                    .map_err(|_| BindError(format!("invalid number {n}")))
            };
            if n.contains(['.', 'e', 'E']) {
                float()?
            } else {
                // Integers too large for BIGINT become DOUBLE rather than failing.
                n.parse().map(Value::Int64).or_else(|_| float())?
            }
        }
        Literal::String(s) => Value::Utf8(s.clone()),
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Null => Value::Null,
        Literal::Date(s) => Value::Date(
            parse_date(s)
                .ok_or_else(|| BindError(format!("invalid input syntax for type date: \"{s}\"")))?,
        ),
        Literal::Interval { value, unit } => {
            let n: i32 = value.trim().parse().map_err(|_| {
                BindError(format!(
                    "invalid input syntax for type interval: \"{value}\""
                ))
            })?;
            let overflow = || BindError(format!("interval '{value}' {unit} is out of range"));
            match unit {
                DateTimeField::Year => Value::Interval {
                    months: n.checked_mul(12).ok_or_else(overflow)?,
                    days: 0,
                },
                DateTimeField::Month => Value::Interval { months: n, days: 0 },
                DateTimeField::Day => Value::Interval { months: 0, days: n },
                other => {
                    return err(format!(
                        "INTERVAL {other} is not supported (no TIMESTAMP type yet)"
                    ))
                }
            }
        }
    };
    Ok(BoundExpr::literal(v))
}

fn binary(l: BoundExpr, op: BinaryOp, r: BoundExpr, ty: Type) -> BoundExpr {
    BoundExpr::new(
        ExprKind::Binary {
            left: Box::new(l),
            op,
            right: Box::new(r),
        },
        ty,
    )
}

fn not(e: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: Box::new(e),
        },
        Type::Boolean,
    )
}

/// Converts `e` to `to`, folding conversions of literals at bind time.
/// Callers are responsible for checking that the conversion is legal.
fn cast(e: BoundExpr, to: Type) -> BoundExpr {
    let from = e.ty;
    if from == to {
        return e;
    }
    match e.kind {
        ExprKind::Literal(Value::Null) => BoundExpr::new(ExprKind::Literal(Value::Null), to),
        ExprKind::Literal(Value::Int64(v)) if to == Type::Float64 => {
            BoundExpr::literal(Value::Float64(v as f64))
        }
        kind => BoundExpr::new(
            ExprKind::Cast {
                expr: Box::new(BoundExpr::new(kind, from)),
            },
            to,
        ),
    }
}

/// Implicit conversion; errors with `what` if the types are incompatible.
fn coerce(e: BoundExpr, to: Type, what: &str) -> Result<BoundExpr> {
    if e.ty.can_coerce_to(to) {
        Ok(cast(e, to))
    } else {
        err(format!("{what} must be type {to}, not type {}", e.ty))
    }
}

/// Explicit CAST rules.
fn castable(from: Type, to: Type) -> bool {
    use Type::*;
    from == to
        || from == Null
        || (to == Utf8 && from != Interval)
        || (from == Utf8 && !matches!(to, Interval | Null))
        || (from.is_numeric() && to.is_numeric())
        || matches!((from, to), (Boolean, Int64) | (Int64, Boolean))
}

/// A string literal compared with a DATE is read as a date, as in Postgres:
/// `o_orderdate < '1995-03-15'`.
fn adapt_literal(other: Type, e: BoundExpr) -> Result<BoundExpr> {
    if other == Type::Date {
        if let ExprKind::Literal(Value::Utf8(s)) = &e.kind {
            return parse_date(s)
                .map(|d| BoundExpr::literal(Value::Date(d)))
                .ok_or_else(|| BindError(format!("invalid input syntax for type date: \"{s}\"")));
        }
    }
    Ok(e)
}

fn op_error(op: BinaryOp, l: Type, r: Type) -> BindError {
    BindError(format!("operator does not exist: {l} {op} {r}"))
}

fn bind_unary(op: UnaryOp, inner: BoundExpr) -> Result<BoundExpr> {
    match op {
        UnaryOp::Not => Ok(not(coerce(inner, Type::Boolean, "argument of NOT")?)),
        UnaryOp::Plus | UnaryOp::Minus => {
            if !(inner.ty.is_numeric() || matches!(inner.ty, Type::Null | Type::Interval)) {
                let sym = if op == UnaryOp::Minus { "-" } else { "+" };
                return err(format!("operator does not exist: {sym}{}", inner.ty));
            }
            if op == UnaryOp::Plus {
                return Ok(inner);
            }
            let ty = inner.ty;
            // Fold negative literals so `-1` is a constant, not an expression.
            Ok(match inner.kind {
                ExprKind::Literal(Value::Int64(v)) => BoundExpr::literal(Value::Int64(-v)),
                ExprKind::Literal(Value::Float64(v)) => BoundExpr::literal(Value::Float64(-v)),
                ExprKind::Literal(Value::Interval { months, days }) => {
                    BoundExpr::literal(Value::Interval {
                        months: -months,
                        days: -days,
                    })
                }
                kind => BoundExpr::new(
                    ExprKind::Unary {
                        op: UnaryOp::Minus,
                        expr: Box::new(BoundExpr::new(kind, ty)),
                    },
                    ty,
                ),
            })
        }
    }
}

fn bind_comparison(op: BinaryOp, l: BoundExpr, r: BoundExpr) -> Result<BoundExpr> {
    let (lt, rt) = (l.ty, r.ty);
    let l = adapt_literal(rt, l)?;
    let r = adapt_literal(lt, r)?;
    let target = Type::common(l.ty, r.ty).ok_or_else(|| op_error(op, l.ty, r.ty))?;
    Ok(binary(cast(l, target), op, cast(r, target), Type::Boolean))
}

fn bind_numeric(op: BinaryOp, l: BoundExpr, r: BoundExpr) -> Result<BoundExpr> {
    let numeric = |t: Type| t.is_numeric() || t == Type::Null;
    if !numeric(l.ty) || !numeric(r.ty) {
        return Err(op_error(op, l.ty, r.ty));
    }
    let target = Type::common(l.ty, r.ty).expect("numeric types always unify");
    Ok(binary(cast(l, target), op, cast(r, target), target))
}

fn bind_binary(op: BinaryOp, l: BoundExpr, r: BoundExpr) -> Result<BoundExpr> {
    use BinaryOp::*;
    match op {
        And | Or => {
            let what = format!("argument of {op}");
            let l = coerce(l, Type::Boolean, &what)?;
            let r = coerce(r, Type::Boolean, &what)?;
            Ok(binary(l, op, r, Type::Boolean))
        }
        Eq | NotEq | Lt | LtEq | Gt | GtEq => bind_comparison(op, l, r),
        Concat => {
            let texty = l.ty == Type::Utf8
                || r.ty == Type::Utf8
                || (l.ty == Type::Null && r.ty == Type::Null);
            if !texty || l.ty == Type::Interval || r.ty == Type::Interval {
                return Err(op_error(op, l.ty, r.ty));
            }
            Ok(binary(
                cast(l, Type::Utf8),
                op,
                cast(r, Type::Utf8),
                Type::Utf8,
            ))
        }
        Plus | Minus => {
            use Type::{Date, Int64, Interval};
            let ty = match (l.ty, r.ty, op) {
                (Date, Interval | Int64, _) => Some(Date),
                (Interval | Int64, Date, Plus) => Some(Date),
                (Date, Date, Minus) => Some(Int64),
                (Interval, Interval, _) => Some(Interval),
                _ => None,
            };
            match ty {
                Some(ty) => Ok(binary(l, op, r, ty)),
                None => bind_numeric(op, l, r),
            }
        }
        Multiply | Divide | Modulo => bind_numeric(op, l, r),
    }
}
