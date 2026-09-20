//! 스코프마다 FROM·CTE·출력 컬럼을 분리해 별칭이 다른 질의로 새지 않게 한다.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::rc::Rc;

use schemagraph_core::{
    AnalysisState, Diagnostic, Edge, EdgeKind, Evidence, EvidenceLayer, Graph, ObjectAnalysis,
    Origin, SourceLocation, VertexId, VertexKind,
};
use schemagraph_source::document::{CatalogDocument, ObjectDoc};
use sha2::{Digest, Sha256};
use sqlparser::ast::{
    Expr, GroupByExpr, Ident, JoinConstraint, JoinOperator, ObjectName, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Spanned, Statement, TableAlias, TableFactor,
    TableWithJoins, Visit, Visitor, WildcardAdditionalOptions,
};
use sqlparser::dialect::{Dialect, GenericDialect};

type Sources = BTreeSet<VertexId>;
type Ctes = BTreeMap<String, Relation>;

#[derive(Clone, Default)]
struct Column {
    name: String,
    sources: Sources,
    unknown: bool,
    location: Option<SourceLocation>,
}

#[derive(Clone, Default)]
struct Relation {
    aliases: Vec<Vec<String>>,
    columns: Vec<Column>,
    unknown: bool,
}

#[derive(Clone, Default)]
struct Scope {
    relations: Vec<Relation>,
    columns: Vec<Column>,
    outer: Option<Rc<Scope>>,
}

impl Scope {
    fn add(&mut self, relation: Relation) {
        self.columns.extend(relation.columns.clone());
        self.relations.push(relation);
    }
}

/// 객체 검색을 몸체마다 전체 카탈로그 순회로 되풀이하지 않는다.
pub(crate) struct CatalogIndex<'a> {
    tables: BTreeMap<(String, String), Vec<(&'a str, &'a ObjectDoc)>>,
    dialect: &'a str,
    routines: BTreeSet<(String, String)>,
}

impl<'a> CatalogIndex<'a> {
    pub(crate) fn new(doc: &'a CatalogDocument) -> Self {
        let mut tables: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut routines = BTreeSet::new();
        for schema in &doc.schemas {
            for routine in &schema.routines {
                let routine_name = match &routine.member_of {
                    Some(pkg) => format!("{pkg}.{}", routine.name),
                    None => routine.name.clone(),
                };
                routines.insert((
                    catalog_key(&doc.dialect, &schema.name),
                    catalog_key(&doc.dialect, &routine_name),
                ));
            }
            for object in &schema.objects {
                tables
                    .entry((
                        catalog_key(&doc.dialect, &schema.name),
                        catalog_key(&doc.dialect, &object.name),
                    ))
                    .or_default()
                    .push((schema.name.as_str(), object));
            }
        }
        Self {
            tables,
            dialect: &doc.dialect,
            routines,
        }
    }
}

struct Binder<'a, 'd> {
    catalog: &'a CatalogIndex<'d>,
    graph: &'a Graph,
    schema: &'a str,
    reads: BTreeMap<VertexId, BTreeSet<(String, Option<SourceLocation>)>>,
    calls: BTreeMap<VertexId, BTreeSet<Option<SourceLocation>>>,
    diagnostics: Vec<Diagnostic>,
    depth: usize,
}

/// 입력 원문과 parser 버전으로 근거를 재확인할 때 쓸 해시다.
pub(crate) fn body_hash(body: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
}

fn catalog_key(dialect: &str, value: &str) -> String {
    if dialect == "sqlite" {
        value.to_lowercase()
    } else {
        value.to_owned()
    }
}

fn name(dialect: &str, ident: &Ident) -> String {
    if dialect == "sqlite" {
        return ident.value.to_lowercase();
    }
    if ident.quote_style.is_some() {
        return ident.value.clone();
    }
    match dialect {
        "postgres" | "postgresql" => ident.value.to_lowercase(),
        "oracle" | "db2" => ident.value.to_uppercase(),
        _ => ident.value.clone(),
    }
}

fn location(item: &impl Spanned) -> Option<SourceLocation> {
    let span = item.span();
    (span.start.line > 0 && span.end.line > 0).then_some(SourceLocation {
        line: span.start.line,
        column: span.start.column,
        end_line: span.end.line,
        end_column: span.end.column,
    })
}

impl Binder<'_, '_> {
    fn call(&mut self, function: &ObjectName, source: Option<SourceLocation>) {
        let parts = self.parts(function);
        let default = catalog_key(self.catalog.dialect, self.schema);
        let (schema, routine) = match parts.as_slice() {
            [routine] => (default.clone(), routine.clone()),
            [schema, routine]
                if self
                    .catalog
                    .routines
                    .contains(&(schema.clone(), routine.clone())) =>
            {
                (schema.clone(), routine.clone())
            }
            [pkg, routine]
                if self
                    .catalog
                    .routines
                    .contains(&(default.clone(), format!("{pkg}.{routine}"))) =>
            {
                (default.clone(), format!("{pkg}.{routine}"))
            }
            [schema, routine] => (schema.clone(), routine.clone()),
            [schema, pkg, routine] => (schema.clone(), format!("{pkg}.{routine}")),
            _ => {
                self.note(
                    "SG_CALL_UNRESOLVED",
                    format!("function '{function}' has an unsupported qualified name"),
                    source,
                );
                return;
            }
        };
        let known = self
            .catalog
            .routines
            .contains(&(schema.clone(), routine.clone()));
        if !known
            && super::is_builtin_function(&routine)
            && (parts.len() == 1
                || matches!(schema.as_str(), "pg_catalog" | "SYS" | "SYSIBM" | "SYSFUN"))
        {
            return;
        }
        let hit = if let Some((pkg, member)) = routine.split_once('.') {
            super::resolve_pkg_member(self.graph, &schema, pkg, member, false)
        } else if known {
            super::resolve_routine(self.graph, &schema, &routine, false)
        } else {
            super::RoutineHit::None
        };
        match hit {
            super::RoutineHit::One(id) => {
                self.calls.entry(id).or_default().insert(source);
            }
            super::RoutineHit::Ambiguous(count) => self.note(
                "SG_CALL_AMBIGUOUS",
                format!("function '{function}' has {count} matching overloads; no target guessed"),
                source,
            ),
            super::RoutineHit::None => self.note(
                "SG_CALL_UNRESOLVED",
                format!("function '{function}' was not resolved in the collected catalog"),
                source,
            ),
        }
    }
    fn note(&mut self, code: &str, message: String, location: Option<SourceLocation>) {
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            message,
            location,
        });
    }

    fn read(&mut self, id: VertexId, role: &str, source: Option<SourceLocation>) {
        self.reads
            .entry(id)
            .or_default()
            .insert((role.to_owned(), source));
    }

    fn query(&mut self, query: &Query, inherited: &Ctes, outer: Option<Rc<Scope>>) -> Relation {
        if self.depth >= 64 {
            self.note(
                "SG_SCOPE_DEPTH",
                "query nesting exceeds the 64-scope analysis limit".into(),
                location(query),
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        }
        self.depth += 1;
        let mut ctes = inherited.clone();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let key = name(self.catalog.dialect, &cte.alias.name);
                if with.recursive {
                    ctes.insert(
                        key.clone(),
                        Relation {
                            unknown: true,
                            ..Relation::default()
                        },
                    );
                }
                let mut relation = self.query(&cte.query, &ctes, outer.clone());
                self.alias(&mut relation, &cte.alias);
                ctes.insert(key, relation);
            }
        }
        let (result, scope) = self.set(&query.body, &ctes, outer);
        if let Some(order) = &query.order_by {
            self.visit(order, &scope, &ctes, "ordering", &result.columns);
        }
        if let Some(limit) = &query.limit_clause {
            self.visit(limit, &scope, &ctes, "limit", &[]);
        }
        if let Some(fetch) = &query.fetch {
            self.visit(fetch, &scope, &ctes, "limit", &[]);
        }
        if !query.pipe_operators.is_empty() || query.for_clause.is_some() {
            self.note(
                "SG_QUERY_MODIFIER",
                "query result modifiers are not fully interpreted".into(),
                location(query),
            );
        }
        self.depth -= 1;
        result
    }

    fn set(
        &mut self,
        set: &SetExpr,
        ctes: &Ctes,
        outer: Option<Rc<Scope>>,
    ) -> (Relation, Rc<Scope>) {
        match set {
            SetExpr::Select(select) => self.select(select, ctes, outer),
            SetExpr::Query(query) => {
                let result = self.query(query, ctes, outer.clone());
                let scope = Rc::new(Scope {
                    columns: result.columns.clone(),
                    outer,
                    ..Scope::default()
                });
                (result, scope)
            }
            SetExpr::SetOperation {
                op, left, right, ..
            } => {
                let (mut left, _) = self.set(left, ctes, outer.clone());
                let (right, _) = self.set(right, ctes, outer.clone());
                if left.columns.len() != right.columns.len() {
                    self.note(
                        "SG_SET_WIDTH",
                        "set-operation column counts differ; output lineage is partial".into(),
                        None,
                    );
                    left.unknown = true;
                }
                for (l, r) in left.columns.iter_mut().zip(right.columns) {
                    if matches!(op, sqlparser::ast::SetOperator::Union) {
                        l.sources.extend(r.sources);
                    }
                    l.unknown |= r.unknown;
                }
                left.unknown |= right.unknown;
                let scope = Rc::new(Scope {
                    columns: left.columns.clone(),
                    outer,
                    ..Scope::default()
                });
                (left, scope)
            }
            SetExpr::Values(values) => {
                let scope = Rc::new(Scope {
                    outer,
                    ..Scope::default()
                });
                let mut columns = Vec::new();
                for row in &values.rows {
                    for (i, expr) in row.iter().enumerate() {
                        if i >= columns.len() {
                            columns.push(Column {
                                name: format!("column{}", i + 1),
                                ..Column::default()
                            });
                        }
                        columns[i].sources.extend(self.visit(
                            expr,
                            &scope,
                            ctes,
                            "projection",
                            &[],
                        ));
                    }
                }
                (
                    Relation {
                        columns,
                        ..Relation::default()
                    },
                    scope,
                )
            }
            _ => {
                self.note(
                    "SG_SET_UNSUPPORTED",
                    "this query body is not supported for column analysis".into(),
                    None,
                );
                (
                    Relation {
                        unknown: true,
                        ..Relation::default()
                    },
                    Rc::new(Scope {
                        outer,
                        ..Scope::default()
                    }),
                )
            }
        }
    }

    fn select(
        &mut self,
        select: &Select,
        ctes: &Ctes,
        outer: Option<Rc<Scope>>,
    ) -> (Relation, Rc<Scope>) {
        let mut scope = Scope {
            outer,
            ..Scope::default()
        };
        for from in &select.from {
            self.join(from, &mut scope, ctes);
        }
        let scope = Rc::new(scope);
        let mut columns = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    let alias = match item {
                        SelectItem::ExprWithAlias { alias, .. } => {
                            name(self.catalog.dialect, alias)
                        }
                        _ => match expr {
                            Expr::Identifier(id) => name(self.catalog.dialect, id),
                            Expr::CompoundIdentifier(ids) => ids
                                .last()
                                .map(|id| name(self.catalog.dialect, id))
                                .unwrap_or_default(),
                            _ => expr.to_string(),
                        },
                    };
                    let before = self.diagnostics.len();
                    let sources = self.visit(expr, &scope, ctes, "projection", &[]);
                    columns.push(Column {
                        name: alias,
                        sources,
                        unknown: self.diagnostics.len() > before,
                        location: location(expr),
                    });
                }
                SelectItem::Wildcard(options) => {
                    if scope.relations.is_empty() || scope.relations.iter().any(|r| r.unknown) {
                        self.note(
                            "SG_WILDCARD_UNKNOWN",
                            "wildcard includes an unresolved relation or has no FROM scope".into(),
                            location(item),
                        );
                    }
                    columns.extend(self.wildcard(scope.columns.clone(), options, location(item)));
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(qualifier),
                    options,
                ) => {
                    let key = self.parts(qualifier);
                    let matches: Vec<_> = scope
                        .relations
                        .iter()
                        .filter(|r| r.aliases.contains(&key))
                        .collect();
                    if matches.len() == 1 && !matches[0].unknown {
                        columns.extend(self.wildcard(
                            matches[0].columns.clone(),
                            options,
                            location(item),
                        ));
                    } else {
                        self.note(
                            "SG_WILDCARD_UNKNOWN",
                            format!("cannot uniquely expand {qualifier}.*"),
                            location(item),
                        );
                    }
                }
                _ => self.note(
                    "SG_WILDCARD_EXPRESSION",
                    "expression wildcards are not supported".into(),
                    location(item),
                ),
            }
        }
        for predicate in [&select.selection, &select.prewhere].into_iter().flatten() {
            self.visit(predicate, &scope, ctes, "predicate", &[]);
        }
        if let Some(having) = &select.having {
            let aliases = if matches!(self.catalog.dialect, "mysql" | "mariadb") {
                columns.as_slice()
            } else {
                &[]
            };
            self.visit(having, &scope, ctes, "predicate", aliases);
        }
        if let Some(qualify) = &select.qualify {
            self.visit(qualify, &scope, ctes, "predicate", &columns);
        }
        if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
            for expr in exprs {
                self.visit(expr, &scope, ctes, "grouping", &columns);
            }
        }
        self.visit(&select.distinct, &scope, ctes, "distinct", &[]);
        self.visit(&select.named_window, &scope, ctes, "window", &[]);
        self.visit(&select.cluster_by, &scope, ctes, "ordering", &[]);
        self.visit(&select.distribute_by, &scope, ctes, "ordering", &[]);
        self.visit(&select.sort_by, &scope, ctes, "ordering", &[]);
        if select.connect_by.is_some()
            || !select.lateral_views.is_empty()
            || select.exclude.is_some()
            || select.value_table_mode.is_some()
        {
            self.note(
                "SG_SELECT_MODIFIER",
                "SELECT modifiers are not fully interpreted".into(),
                location(select),
            );
        }
        let unknown = scope.relations.iter().any(|r| r.unknown);
        (
            Relation {
                columns,
                unknown,
                ..Relation::default()
            },
            scope,
        )
    }

    fn wildcard(
        &mut self,
        mut columns: Vec<Column>,
        options: &WildcardAdditionalOptions,
        source: Option<SourceLocation>,
    ) -> Vec<Column> {
        if options.to_string().trim().is_empty() {
            for column in &mut columns {
                column.location = source.clone();
                for id in &column.sources {
                    self.read(id.clone(), "projection", source.clone());
                }
            }
            return columns;
        }
        self.note(
            "SG_WILDCARD_MODIFIER",
            "wildcard EXCEPT/REPLACE/RENAME modifiers are not yet resolved; output lineage omitted"
                .into(),
            source,
        );
        Vec::new()
    }

    fn join(&mut self, twj: &TableWithJoins, scope: &mut Scope, ctes: &Ctes) {
        let mut group = Scope {
            outer: scope.outer.clone(),
            ..Scope::default()
        };
        self.add_factor(&twj.relation, &mut group, ctes, Rc::new(scope.clone()));
        for join in &twj.joins {
            let mut lateral_scope = scope.clone();
            for r in &group.relations {
                lateral_scope.add(r.clone());
            }
            let mut right = Scope {
                outer: scope.outer.clone(),
                ..Scope::default()
            };
            self.add_factor(&join.relation, &mut right, ctes, Rc::new(lateral_scope));
            let constraint = match &join.join_operator {
                JoinOperator::Join(c)
                | JoinOperator::Inner(c)
                | JoinOperator::Left(c)
                | JoinOperator::Right(c)
                | JoinOperator::LeftOuter(c)
                | JoinOperator::RightOuter(c)
                | JoinOperator::FullOuter(c)
                | JoinOperator::Semi(c)
                | JoinOperator::LeftSemi(c)
                | JoinOperator::RightSemi(c)
                | JoinOperator::Anti(c)
                | JoinOperator::LeftAnti(c)
                | JoinOperator::RightAnti(c)
                | JoinOperator::StraightJoin(c)
                | JoinOperator::AsOf { constraint: c, .. } => Some(c),
                _ => None,
            };
            let keys = match constraint {
                Some(JoinConstraint::Using(names)) => {
                    names.iter().map(|n| self.parts(n).join(".")).collect()
                }
                Some(JoinConstraint::Natural) => group
                    .columns
                    .iter()
                    .filter(|l| right.columns.iter().any(|r| r.name == l.name))
                    .map(|c| c.name.clone())
                    .collect(),
                _ => Vec::new(),
            };
            if keys.is_empty() {
                group.columns.extend(right.columns);
                group.relations.extend(right.relations);
            } else {
                self.using(&mut group, right, &keys);
            }
            if let Some(JoinConstraint::On(expr)) = constraint {
                let mut on_scope = scope.clone();
                for r in &group.relations {
                    on_scope.add(r.clone());
                }
                self.visit(expr, &Rc::new(on_scope), ctes, "join", &[]);
            }
            if let JoinOperator::AsOf {
                match_condition, ..
            } = &join.join_operator
            {
                let mut on_scope = scope.clone();
                for r in &group.relations {
                    on_scope.add(r.clone());
                }
                self.visit(match_condition, &Rc::new(on_scope), ctes, "join", &[]);
            }
            if matches!(
                join.join_operator,
                JoinOperator::Semi(_)
                    | JoinOperator::LeftSemi(_)
                    | JoinOperator::RightSemi(_)
                    | JoinOperator::Anti(_)
                    | JoinOperator::LeftAnti(_)
                    | JoinOperator::RightAnti(_)
                    | JoinOperator::CrossApply
                    | JoinOperator::OuterApply
            ) {
                self.note(
                    "SG_JOIN_PROJECTION",
                    "SEMI/ANTI/APPLY output scope is not fully interpreted; output lineage omitted"
                        .into(),
                    None,
                );
            }
        }
        scope.columns.extend(group.columns);
        scope.relations.extend(group.relations);
    }

    fn add_factor(
        &mut self,
        factor: &TableFactor,
        group: &mut Scope,
        ctes: &Ctes,
        visible: Rc<Scope>,
    ) {
        if let TableFactor::NestedJoin {
            table_with_joins,
            alias: None,
        } = factor
        {
            self.join(table_with_joins, group, ctes);
        } else {
            group.add(self.factor(factor, ctes, visible));
        }
    }

    fn using(&mut self, scope: &mut Scope, right: Scope, keys: &[String]) {
        let mut merged = Vec::new();
        for key in keys {
            let left: Vec<_> = scope.columns.iter().filter(|c| &c.name == key).collect();
            let rhs: Vec<_> = right.columns.iter().filter(|c| &c.name == key).collect();
            if left.len() == 1 && rhs.len() == 1 {
                let mut col = left[0].clone();
                col.sources.extend(rhs[0].sources.clone());
                col.unknown |= rhs[0].unknown;
                for id in &col.sources {
                    self.read(id.clone(), "join", None);
                }
                merged.push(col);
            } else {
                self.note(
                    "SG_USING_AMBIGUOUS",
                    format!("JOIN USING column '{key}' is missing or ambiguous"),
                    None,
                );
            }
        }
        if self.catalog.dialect == "sqlite" {
            for col in &mut scope.columns {
                if let Some(merged) = merged.iter().find(|m| m.name == col.name) {
                    *col = merged.clone();
                }
            }
        } else {
            let mut columns = merged;
            columns.extend(
                scope
                    .columns
                    .iter()
                    .filter(|c| !keys.contains(&c.name))
                    .cloned(),
            );
            scope.columns = columns;
        }
        scope.columns.extend(
            right
                .columns
                .iter()
                .filter(|c| !keys.contains(&c.name))
                .cloned(),
        );
        scope.relations.extend(right.relations);
    }

    fn parts(&self, object: &ObjectName) -> Vec<String> {
        object
            .0
            .iter()
            .filter_map(|p| p.as_ident())
            .map(|id| name(self.catalog.dialect, id))
            .collect()
    }

    fn factor(&mut self, factor: &TableFactor, ctes: &Ctes, scope: Rc<Scope>) -> Relation {
        match factor {
            TableFactor::Table {
                name: table,
                alias,
                args,
                ..
            } if args.is_none() => {
                let parts = self.parts(table);
                let mut result = if parts.len() == 1 && ctes.contains_key(&parts[0]) {
                    ctes[&parts[0]].clone()
                } else {
                    self.table(&parts, location(factor))
                };
                if let Some(alias) = alias {
                    self.alias(&mut result, alias);
                } else {
                    result.aliases = vec![parts.clone()];
                    if let Some(last) = parts.last() {
                        result.aliases.push(vec![last.clone()]);
                    }
                }
                result
            }
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
            } => {
                let outer = if *lateral {
                    Some(scope)
                } else {
                    scope.outer.clone()
                };
                let mut result = self.query(subquery, ctes, outer);
                if let Some(alias) = alias {
                    self.alias(&mut result, alias);
                }
                result
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                let mut nested = Scope {
                    outer: scope.outer.clone(),
                    ..Scope::default()
                };
                self.join(table_with_joins, &mut nested, ctes);
                let mut result = Relation {
                    columns: nested.columns,
                    unknown: nested.relations.iter().any(|r| r.unknown),
                    aliases: nested
                        .relations
                        .into_iter()
                        .flat_map(|r| r.aliases)
                        .collect(),
                };
                // 별칭 없는 괄호 JOIN은 factor 하나로 압축하면 a.x와 b.x가
                // 섞인다. join 호출부에서 별도 경로로 펼친다.
                if let Some(alias) = alias {
                    self.alias(&mut result, alias);
                }
                result
            }
            _ => {
                self.note("SG_RELATION_UNSUPPORTED","table function, pivot, or other relation form is not supported for column resolution".into(),location(factor));
                Relation {
                    unknown: true,
                    ..Relation::default()
                }
            }
        }
    }

    fn table(&mut self, parts: &[String], source: Option<SourceLocation>) -> Relation {
        let (schema, table) = match parts {
            [table] => (
                catalog_key(self.catalog.dialect, self.schema),
                table.clone(),
            ),
            [schema, table] => (schema.clone(), table.clone()),
            _ => {
                self.note("SG_CROSS_DATABASE",format!("relation '{}' needs a database/catalog namespace; no local target inferred",parts.join(".")),source);
                return Relation {
                    unknown: true,
                    ..Relation::default()
                };
            }
        };
        let matches = self
            .catalog
            .tables
            .get(&(schema.clone(), table.clone()))
            .cloned()
            .unwrap_or_default();
        if matches.len() != 1 {
            self.note(
                "SG_RELATION_UNRESOLVED",
                format!("relation '{schema}.{table}' is missing or ambiguous in the catalog"),
                source,
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        }
        let (actual_schema, object) = matches[0];
        let kind = match object.kind.as_str() {
            "view" => VertexKind::View,
            "materialized-view" => VertexKind::MaterializedView,
            "synonym" => VertexKind::Synonym,
            "sequence" => VertexKind::Sequence,
            _ => VertexKind::Table,
        };
        let Some(id) = super::resolve_object(
            self.graph,
            actual_schema,
            &object.name,
            kind,
            &object.kind,
            false,
        ) else {
            self.note(
                "SG_RELATION_UNRESOLVED",
                format!(
                    "catalog relation '{actual_schema}.{}' has no matching graph vertex",
                    object.name
                ),
                source,
            );
            return Relation {
                unknown: true,
                ..Relation::default()
            };
        };
        self.read(id, "relation", source);
        let mut columns: Vec<_> = object.columns.iter().collect();
        columns.sort_by_key(|c| c.ordinal);
        let columns = columns
            .into_iter()
            .map(|column| {
                let sources = super::resolve_member(
                    self.graph,
                    actual_schema,
                    &object.name,
                    &column.name,
                    VertexKind::Column,
                    "column",
                    false,
                )
                .into_iter()
                .collect::<Sources>();
                Column {
                    name: catalog_key(self.catalog.dialect, &column.name),
                    unknown: sources.is_empty(),
                    sources,
                    location: None,
                }
            })
            .collect();
        Relation {
            columns,
            ..Relation::default()
        }
    }

    fn alias(&mut self, relation: &mut Relation, alias: &TableAlias) {
        relation.aliases = vec![vec![name(self.catalog.dialect, &alias.name)]];
        if alias.columns.len() > relation.columns.len() {
            relation.unknown = true;
            self.note(
                "SG_ALIAS_WIDTH",
                "column aliases exceed the known output width".into(),
                None,
            );
        }
        for (column, alias) in relation.columns.iter_mut().zip(&alias.columns) {
            column.name = name(self.catalog.dialect, &alias.name);
        }
    }

    fn resolve(
        &mut self,
        ids: &[Ident],
        scope: &Scope,
        source: Option<SourceLocation>,
        aliases: &[Column],
    ) -> Sources {
        let parts: Vec<_> = ids
            .iter()
            .map(|id| name(self.catalog.dialect, id))
            .collect();
        let Some(column) = parts.last() else {
            return Sources::new();
        };
        let qualifier = &parts[..parts.len() - 1];
        let mut current = Some(scope);
        while let Some(scope) = current {
            let candidates: Vec<_> = if qualifier.is_empty() {
                scope.columns.iter().filter(|c| &c.name == column).collect()
            } else {
                scope
                    .relations
                    .iter()
                    .filter(|r| r.aliases.iter().any(|a| a == qualifier))
                    .flat_map(|r| &r.columns)
                    .filter(|c| &c.name == column)
                    .collect()
            };
            let unknown = if qualifier.is_empty() {
                scope.relations.iter().any(|r| r.unknown)
            } else {
                scope
                    .relations
                    .iter()
                    .any(|r| r.aliases.iter().any(|a| a == qualifier) && r.unknown)
            };
            if candidates.len() == 1 && !unknown && !candidates[0].unknown {
                return candidates[0].sources.clone();
            }
            if !candidates.is_empty() || unknown {
                self.note(
                    "SG_COLUMN_AMBIGUOUS",
                    format!(
                        "column '{}' is ambiguous or has unresolved sources",
                        parts.join(".")
                    ),
                    source,
                );
                return Sources::new();
            }
            // 로컬 별칭이 존재하면 컬럼 누락을 외부 동명 별칭으로 덮지 않는다.
            if !qualifier.is_empty()
                && scope
                    .relations
                    .iter()
                    .any(|r| r.aliases.iter().any(|a| a == qualifier))
            {
                break;
            }
            current = scope.outer.as_deref();
        }
        if qualifier.is_empty() {
            let matched: Vec<_> = aliases.iter().filter(|c| &c.name == column).collect();
            if matched.len() == 1 && !matched[0].unknown {
                return matched[0].sources.clone();
            }
        }
        self.note(
            "SG_COLUMN_UNRESOLVED",
            format!(
                "column '{}' cannot be resolved in its SQL scope",
                parts.join(".")
            ),
            source,
        );
        Sources::new()
    }

    fn visit<T: Visit>(
        &mut self,
        node: &T,
        scope: &Rc<Scope>,
        ctes: &Ctes,
        role: &str,
        aliases: &[Column],
    ) -> Sources {
        let mut visitor = ExprVisitor {
            binder: self,
            scope: scope.clone(),
            ctes,
            role,
            aliases,
            queries: 0,
            expression_depth: 0,
            sources: Sources::new(),
        };
        let _ = node.visit(&mut visitor);
        visitor.sources
    }
}

struct ExprVisitor<'a, 'b, 'c, 'd> {
    binder: &'a mut Binder<'b, 'd>,
    scope: Rc<Scope>,
    ctes: &'c Ctes,
    role: &'c str,
    aliases: &'c [Column],
    queries: usize,
    expression_depth: usize,
    sources: Sources,
}

impl Visitor for ExprVisitor<'_, '_, '_, '_> {
    type Break = ();
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        if self.queries == 0 {
            let result = self
                .binder
                .query(query, self.ctes, Some(self.scope.clone()));
            self.sources
                .extend(result.columns.into_iter().flat_map(|c| c.sources));
        }
        self.queries += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.queries -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if self.queries != 0 {
            return ControlFlow::Continue(());
        }
        let root_expression = self.expression_depth == 0;
        self.expression_depth += 1;
        if let Expr::Function(function) = expr {
            self.binder.call(&function.name, location(expr));
        }
        let ids = match expr {
            Expr::Identifier(id) => std::slice::from_ref(id),
            Expr::CompoundIdentifier(ids) => ids.as_slice(),
            _ => return ControlFlow::Continue(()),
        };
        let aliases = if root_expression || self.role == "predicate" {
            self.aliases
        } else {
            &[]
        };
        let projected: Vec<_> = if root_expression && self.role == "ordering" && ids.len() == 1 {
            aliases
                .iter()
                .filter(|c| c.name == name(self.binder.catalog.dialect, &ids[0]) && !c.unknown)
                .collect()
        } else {
            Vec::new()
        };
        let sources = if projected.len() == 1 {
            projected[0].sources.clone()
        } else {
            self.binder
                .resolve(ids, &self.scope, location(expr), aliases)
        };
        for id in &sources {
            self.binder.read(id.clone(), self.role, location(expr));
        }
        self.sources.extend(sources);
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, _: &Expr) -> ControlFlow<()> {
        if self.queries == 0 {
            self.expression_depth -= 1;
        }
        ControlFlow::Continue(())
    }
}

/// 뷰의 읽기 의존성과 출력 값의 계보를 실제 reader 정점에만 연결한다.
pub(crate) fn enrich_view(
    graph: &mut Graph,
    catalog: &CatalogIndex<'_>,
    schema: &str,
    object: &ObjectDoc,
    owner: &VertexId,
    dialect: Option<&dyn Dialect>,
) -> Vec<String> {
    let Some(body) = object.body.as_ref().filter(|s| !s.trim().is_empty()) else {
        graph.set_analysis(
            owner.clone(),
            ObjectAnalysis {
                state: AnalysisState::Unsupported,
                scope: "column-dependencies-and-lineage".into(),
                body_hash: None,
                diagnostics: vec![Diagnostic {
                    code: "SG_BODY_UNAVAILABLE".into(),
                    message: "view definition was not collected".into(),
                    location: None,
                }],
            },
        );
        return vec![format!(
            "view {owner}: definition unavailable; dependencies were not analyzed"
        )];
    };
    let hash = body_hash(body);
    let default = GenericDialect {};
    let parsed = sqlparser::parser::Parser::parse_sql(dialect.unwrap_or(&default), body);
    let mut binder = Binder {
        catalog,
        graph,
        schema,
        reads: BTreeMap::new(),
        calls: BTreeMap::new(),
        diagnostics: Vec::new(),
        depth: 0,
    };
    let mut outputs = Vec::new();
    match parsed {
        Ok(statements) if statements.len() == 1 => {
            for statement in &statements {
                let query = match statement {
                    Statement::Query(q) | Statement::CreateView { query: q, .. } => q,
                    _ => {
                        binder.note(
                            "SG_VIEW_STATEMENT",
                            "view body is not a SELECT query".into(),
                            None,
                        );
                        continue;
                    }
                };
                outputs = binder.query(query, &Ctes::new(), None).columns;
            }
        }
        Ok(_) => binder.note(
            "SG_VIEW_STATEMENT",
            "a view definition must contain exactly one query".into(),
            None,
        ),
        Err(error) => binder.note(
            "SG_PARSE_ERROR",
            format!("view SQL parse failed: {error}"),
            None,
        ),
    }
    let diagnostics = binder.diagnostics;
    let reads = binder.reads;
    let calls = binder.calls;
    for (to, locations) in calls {
        graph.add_edge(Edge {
            from: owner.clone(),
            to: to.clone(),
            kind: EdgeKind::Calls,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: format!("view {owner} calls {to}"),
            }],
        });
        for location in locations {
            graph.add_origin(
                (owner.clone(), to.clone(), EdgeKind::Calls),
                Origin {
                    body_hash: hash.clone(),
                    role: "call".into(),
                    location,
                },
            );
        }
    }
    for (to, origins) in reads {
        graph.add_edge(Edge {
            from: owner.clone(),
            to: to.clone(),
            kind: EdgeKind::Reads,
            evidence: vec![Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: format!("view {owner} reads {to}"),
            }],
        });
        for (role, location) in origins {
            graph.add_origin(
                (owner.clone(), to.clone(), EdgeKind::Reads),
                Origin {
                    body_hash: hash.clone(),
                    role,
                    location,
                },
            );
        }
    }
    let mut columns: Vec<_> = object.columns.iter().collect();
    columns.sort_by_key(|c| c.ordinal);
    let width_matches = columns.len() == outputs.len();
    let mut diagnostics = diagnostics;
    if !columns.is_empty() && !width_matches {
        diagnostics.push(Diagnostic {
            code: "SG_OUTPUT_WIDTH".into(),
            message: "catalog output width differs from parsed projection; output lineage omitted"
                .into(),
            location: None,
        });
    }
    let output_shape_known = !diagnostics.iter().any(|d| {
        matches!(
            d.code.as_str(),
            "SG_WILDCARD_UNKNOWN"
                | "SG_WILDCARD_MODIFIER"
                | "SG_WILDCARD_EXPRESSION"
                | "SG_SELECT_MODIFIER"
                | "SG_SET_WIDTH"
                | "SG_ALIAS_WIDTH"
                | "SG_SCOPE_DEPTH"
                | "SG_VIEW_STATEMENT"
                | "SG_JOIN_PROJECTION"
        )
    });
    if width_matches && output_shape_known {
        for (column, output) in columns.into_iter().zip(outputs) {
            let Some(from) = super::resolve_member(
                graph,
                schema,
                &object.name,
                &column.name,
                VertexKind::Column,
                "column",
                false,
            ) else {
                continue;
            };
            for to in output.sources {
                graph.add_edge(Edge {
                    from: from.clone(),
                    to: to.clone(),
                    kind: EdgeKind::DerivesFrom,
                    evidence: vec![Evidence {
                        layer: EvidenceLayer::BodyParse,
                        detail: format!("{from} derives its value from {to}"),
                    }],
                });
                graph.add_origin(
                    (from.clone(), to, EdgeKind::DerivesFrom),
                    Origin {
                        body_hash: hash.clone(),
                        role: "value".into(),
                        location: output.location.clone(),
                    },
                );
            }
        }
    }
    let state = if diagnostics.is_empty() {
        AnalysisState::Complete
    } else if diagnostics.iter().any(|d| d.code == "SG_PARSE_ERROR") {
        AnalysisState::Unsupported
    } else {
        AnalysisState::Partial
    };
    let notes = diagnostics
        .iter()
        .map(|d| format!("view {owner}: {}: {}", d.code, d.message))
        .collect();
    graph.set_analysis(
        owner.clone(),
        ObjectAnalysis {
            state,
            scope: if object.columns.is_empty() {
                "column-dependencies"
            } else {
                "column-dependencies-and-lineage"
            }
            .into(),
            body_hash: Some(hash),
            diagnostics,
        },
    );
    notes
}
