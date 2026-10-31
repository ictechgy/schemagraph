//! 루틴 본문의 식별자와 CTE 스코프를 보존해 물리 관계만 수집한다.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, ObjectName, ObjectNamePart, Query, Statement, Visit, VisitMut, Visitor, VisitorMut,
};

use super::{
    is_sqlserver_transition_relation, qualified_write_targets, read_exclusions, relation_name_key,
    relation_name_parts, write_targets_shallow, ParsedTrigger,
};

/// 컬럼 해석과 같은 PostgreSQL 인용 규칙을 본문의 관계·호출 이름에도 적용한다.
pub(super) fn normalize_postgres(statement: &mut Statement) {
    struct Normalize;
    impl Normalize {
        fn name(name: &mut ObjectName) {
            for part in &mut name.0 {
                if let ObjectNamePart::Identifier(ident) = part {
                    ident.value = super::scope::name("postgres", ident);
                }
            }
        }
    }
    impl VisitorMut for Normalize {
        type Break = ();
        fn pre_visit_relation(&mut self, name: &mut ObjectName) -> ControlFlow<()> {
            Self::name(name);
            ControlFlow::Continue(())
        }
        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
            if let Some(with) = &mut query.with {
                for cte in &mut with.cte_tables {
                    cte.alias.name.value = super::scope::name("postgres", &cte.alias.name);
                }
            }
            ControlFlow::Continue(())
        }
        fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
            if let Expr::Function(function) = expr {
                Self::name(&mut function.name);
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<()> {
            if let Statement::Call(call) = statement {
                Self::name(&mut call.name);
            }
            ControlFlow::Continue(())
        }
    }
    let _ = VisitMut::visit(statement, &mut Normalize);
}

/// CTE 정의의 이전 별칭과 본문 별칭을 구분하고 쓰기 제외를 문장 스코프에만 적용한다.
pub(super) fn collect_reads(
    statement: &Statement,
    parsed: &mut ParsedTrigger,
    sqlserver_trigger: bool,
) {
    struct Relations<'a> {
        parsed: &'a mut ParsedTrigger,
        sqlserver_trigger: bool,
        ctes: Vec<BTreeSet<String>>,
        definitions: BTreeMap<usize, BTreeSet<String>>,
        exclusions: Vec<Vec<(Option<String>, String)>>,
        qualified_writes: Vec<Vec<ObjectName>>,
    }
    impl Visitor for Relations<'_> {
        type Break = ();
        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
            self.exclusions.push(read_exclusions(
                statement,
                &write_targets_shallow(statement),
            ));
            self.qualified_writes
                .push(qualified_write_targets(statement));
            ControlFlow::Continue(())
        }
        fn post_visit_statement(&mut self, _: &Statement) -> ControlFlow<()> {
            self.exclusions.pop();
            self.qualified_writes.pop();
            ControlFlow::Continue(())
        }
        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
            let mut visible = self
                .definitions
                .remove(&(query as *const Query as usize))
                .unwrap_or_else(|| self.ctes.last().cloned().unwrap_or_default());
            if let Some(with) = &query.with {
                // RECURSIVE는 뒤 형제 CTE 참조도 허용한다. 비재귀는 이전 별칭만 보인다.
                if with.recursive {
                    visible.extend(
                        with.cte_tables
                            .iter()
                            .map(|cte| cte.alias.name.value.clone()),
                    );
                }
                for cte in &with.cte_tables {
                    let name = cte.alias.name.value.clone();
                    self.definitions
                        .insert(cte.query.as_ref() as *const Query as usize, visible.clone());
                    visible.insert(name);
                }
            }
            self.ctes.push(visible);
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
            self.ctes.pop();
            ControlFlow::Continue(())
        }
        fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<()> {
            if name.0.iter().filter_map(|part| part.as_ident()).count() == 3 {
                let is_write = self
                    .qualified_writes
                    .last()
                    .is_some_and(|targets| targets.contains(name));
                if !is_write && !self.parsed.qualified_reads.contains(name) {
                    self.parsed.qualified_reads.push(name.clone());
                }
                return ControlFlow::Continue(());
            }
            let Some(target) = relation_name_parts(name) else {
                self.parsed
                    .ignored_relations
                    .insert(relation_name_key(name));
                return ControlFlow::Continue(());
            };
            let cte = target.0.is_none()
                && self
                    .ctes
                    .last()
                    .is_some_and(|scope| scope.contains(&target.1));
            let excluded = self
                .exclusions
                .last()
                .is_some_and(|targets| targets.contains(&target));
            if target.1.is_empty()
                || cte
                || excluded
                || (self.sqlserver_trigger && is_sqlserver_transition_relation(&target))
                || self.parsed.local_relations.contains(&target)
                || self.parsed.reads.contains(&target)
            {
                return ControlFlow::Continue(());
            }
            self.parsed.reads.push(target);
            ControlFlow::Continue(())
        }
    }
    let _ = Visit::visit(
        statement,
        &mut Relations {
            parsed,
            sqlserver_trigger,
            ctes: Vec::new(),
            definitions: BTreeMap::new(),
            exclusions: Vec::new(),
            qualified_writes: Vec::new(),
        },
    );
}
