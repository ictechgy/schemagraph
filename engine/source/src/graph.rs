//! CatalogDocument → Graph 변환.
//!
//! 여기서 하는 일은 오직 카탈로그 사실의 그래프화다. 이름 해석(스키마 미상
//! 참조 등)은 정직하게 처리하고, 못 보는 것은 limitations로 넘긴다 —
//! 추측으로 간선을 만들지 않는다.

use schemagraph_core::{
    ColumnMetadata, Edge, EdgeKind, Evidence, EvidenceLayer, ForeignKeyMetadata, Graph,
    IndexMetadata, SchemaMetadata, Usage, Vertex, VertexId, VertexKind,
};
use std::collections::BTreeMap;

use crate::document::*;

/// document를 그래프로 바꾼다. document의 limitations를 그대로 계승한다.
pub fn document_to_graph(doc: &CatalogDocument) -> Graph {
    let mut g = Graph::new();
    for l in &doc.limitations {
        g.add_limitation(l.clone());
    }
    let mut packages = PackageIndex::default();
    for schema in &doc.schemas {
        build_schema(&mut g, schema, &mut packages);
    }
    // 같은 테이블 쌍·컬럼 쌍의 FK가 여럿이면 같은 키의 간선이 반복되므로, 모든 스키마의
    // FK 간선을 모아 add_edges로 근거를 키마다 한 번에 합친다.
    let mut foreign_keys = Vec::new();
    for schema in &doc.schemas {
        collect_foreign_key_edges(&mut g, schema, &mut foreign_keys);
    }
    g.add_edges(foreign_keys);
    attach_schema_metadata(&mut g, doc);
    crate::dependencies::apply(&mut g, doc);
    g
}

/// 새 reader가 명시한 메타데이터만 실제 graph 정점으로 정규화한다.
fn attach_schema_metadata(graph: &mut Graph, doc: &CatalogDocument) {
    if doc.context.is_none()
        && !doc.schemas.iter().any(|schema| {
            schema
                .objects
                .iter()
                .flat_map(|object| &object.indexes)
                .any(|index| index.definition_complete.is_some() || index.predicate.is_some())
                || schema
                    .objects
                    .iter()
                    .any(|object| object.columns.iter().any(|column| column.pk_position > 0))
        })
    {
        return;
    }
    let mut metadata = SchemaMetadata::default();
    for schema in &doc.schemas {
        for object in &schema.objects {
            let table = VertexId::object(&schema.name, &object.name);
            let primary_key = declared_primary_key(object);
            for column in &object.columns {
                if let Some(id) = member_id(graph, &table, &column.name, VertexKind::Column) {
                    metadata.columns.insert(
                        id,
                        ColumnMetadata {
                            data_type: column.data_type.clone(),
                            nullable: column.nullable,
                            ordinal: column.ordinal,
                            pk_position: pk_position(column, primary_key),
                        },
                    );
                }
            }
            for index in &object.indexes {
                let Some(index_id) = member_id(graph, &table, &index.name, VertexKind::Index)
                else {
                    continue;
                };
                let columns = index
                    .columns
                    .iter()
                    .filter_map(|name| member_id(graph, &table, name, VertexKind::Column))
                    .collect::<Vec<_>>();
                let complete = index.definition_complete == Some(true)
                    && columns.len() == index.columns.len()
                    && columns.iter().all(|column| {
                        graph
                            .vertex(column)
                            .is_some_and(|vertex| vertex.kind == VertexKind::Column)
                    });
                metadata.indexes.insert(
                    index_id,
                    IndexMetadata {
                        table: table.clone(),
                        columns,
                        unique: index.unique,
                        has_predicate: index.has_predicate == Some(true)
                            || index.predicate.is_some(),
                        complete,
                    },
                );
            }
            for constraint in object
                .constraints
                .iter()
                .filter(|constraint| constraint.kind == "fk")
            {
                let Some(constraint_id) =
                    member_id(graph, &table, &constraint.name, VertexKind::Constraint)
                else {
                    continue;
                };
                let columns = constraint
                    .columns
                    .iter()
                    .filter_map(|name| member_id(graph, &table, name, VertexKind::Column))
                    .collect::<Vec<_>>();
                let (target_table, target_columns, expected_targets) = match &constraint.referenced
                {
                    Some(reference) => {
                        let target_schema = reference.schema.as_deref().unwrap_or(&schema.name);
                        let table_id = VertexId::object(target_schema, &reference.table);
                        let names = referenced_column_names(doc, target_schema, reference);
                        let target_columns = names
                            .iter()
                            .filter_map(|name| {
                                member_id(graph, &table_id, name, VertexKind::Column)
                            })
                            .collect::<Vec<_>>();
                        let table = graph.vertex(&table_id).map(|_| table_id);
                        (table, target_columns, names.len())
                    }
                    None => (None, Vec::new(), 0),
                };
                let complete = doc
                    .context
                    .as_ref()
                    .is_some_and(|context| context.catalog_complete)
                    && !columns.is_empty()
                    && columns.len() == constraint.columns.len()
                    && columns.iter().all(|column| {
                        graph
                            .vertex(column)
                            .is_some_and(|vertex| vertex.kind == VertexKind::Column)
                    })
                    && target_table.is_some()
                    && target_columns.len() == expected_targets
                    && target_columns.len() == columns.len()
                    && target_columns.iter().all(|column| {
                        graph
                            .vertex(column)
                            .is_some_and(|vertex| vertex.kind == VertexKind::Column)
                    });
                metadata.foreign_keys.insert(
                    constraint_id,
                    ForeignKeyMetadata {
                        table: table.clone(),
                        columns,
                        target_table,
                        target_columns,
                        complete,
                    },
                );
            }
        }
    }
    metadata.catalog_complete = doc
        .context
        .as_ref()
        .is_some_and(|context| context.catalog_complete);
    graph.set_schema_metadata(metadata);
}

/// FK가 가리키는 대상 컬럼 이름이다.
///
/// 대상 컬럼을 생략한 참조(`REFERENCES parent`)는 SQL 규칙대로 대상 테이블의
/// PK 키 순서를 쓴다. SQLite 수집기는 이 경우 빈 이름을 옮기므로, 이름이 모두
/// 비었을 때만 PK로 해석한다. 대상 테이블이나 PK를 모르면 빈 목록이다.
fn referenced_column_names(
    doc: &CatalogDocument,
    schema: &str,
    reference: &ReferencedDoc,
) -> Vec<String> {
    if reference.columns.iter().any(|name| !name.is_empty()) {
        return reference.columns.clone();
    }
    let Some(target) = doc
        .schemas
        .iter()
        .filter(|candidate| candidate.name == schema)
        .flat_map(|candidate| &candidate.objects)
        .find(|object| object.name == reference.table)
    else {
        return Vec::new();
    };
    let primary_key = declared_primary_key(target);
    let mut keyed: Vec<_> = target
        .columns
        .iter()
        .map(|column| (pk_position(column, primary_key), column.name.clone()))
        .filter(|(position, _)| *position > 0)
        .collect();
    keyed.sort();
    keyed.into_iter().map(|(_, name)| name).collect()
}

/// 컬럼의 PK 위치를 정하는 근거가 될 PK 제약이다.
///
/// 생산자가 `pk_position`을 하나라도 채웠으면 그 값을 믿고 `None`이다. 모두 0인데
/// PK 제약이 있으면 제약의 키 순서를 쓴다 — 옛 PostgreSQL 수집기처럼 위치를 빠뜨린
/// 문서도 "PK 없음"으로 오판하지 않게 판정 권위인 엔진에서 한 번 더 보정한다.
fn declared_primary_key(object: &ObjectDoc) -> Option<&ConstraintDoc> {
    if object.columns.iter().any(|column| column.pk_position > 0) {
        return None;
    }
    object
        .constraints
        .iter()
        .find(|constraint| constraint.kind == "pk")
}

/// 문서 값 또는 PK 제약 키 순서에서 컬럼의 1-based PK 위치를 구한다.
fn pk_position(column: &ColumnDoc, primary_key: Option<&ConstraintDoc>) -> u32 {
    primary_key
        .and_then(|constraint| {
            constraint
                .columns
                .iter()
                .position(|name| *name == column.name)
        })
        .map_or(column.pk_position, |index| index as u32 + 1)
}

fn member_id(graph: &Graph, table: &VertexId, name: &str, kind: VertexKind) -> Option<VertexId> {
    graph
        .outgoing(table)
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Contains)
        .filter_map(|edge| graph.vertex(&edge.to))
        .find(|vertex| vertex.name == name && vertex.kind == kind)
        .map(|vertex| vertex.id.clone())
}

fn build_schema(g: &mut Graph, schema: &SchemaDoc, packages: &mut PackageIndex) {
    let schema_id = VertexId::schema(&schema.name);
    g.add_vertex(Vertex {
        id: schema_id.clone(),
        kind: VertexKind::Schema,
        name: schema.name.clone(),
        schema: schema.name.clone(),
    });

    for obj in &schema.objects {
        let obj_id = VertexId::object(&schema.name, &obj.name);
        let kind = match obj.kind.as_str() {
            "view" => VertexKind::View,
            "materialized-view" => VertexKind::MaterializedView,
            "sequence" => VertexKind::Sequence,
            "type" => VertexKind::Type,
            "synonym" => VertexKind::Synonym,
            _ => VertexKind::Table,
        };
        g.add_vertex(Vertex {
            id: obj_id.clone(),
            kind,
            name: obj.name.clone(),
            schema: schema.name.clone(),
        });
        g.add_edge(contains(&schema_id, &obj_id));
        if let Some(u) = &obj.usage {
            g.set_usage(obj_id.clone(), to_usage(u));
        }

        for col in &obj.columns {
            add_member(
                g,
                &obj_id,
                &schema.name,
                &obj.name,
                &col.name,
                VertexKind::Column,
            );
        }

        for con in &obj.constraints {
            add_member(
                g,
                &obj_id,
                &schema.name,
                &obj.name,
                &con.name,
                VertexKind::Constraint,
            );
        }

        for idx in &obj.indexes {
            if let Some(idx_id) = add_member(
                g,
                &obj_id,
                &schema.name,
                &obj.name,
                &idx.name,
                VertexKind::Index,
            ) {
                if let Some(u) = &idx.usage {
                    g.set_usage(idx_id, to_usage(u));
                }
            }
        }

        for trg in &obj.triggers {
            let Some(trg_id) = add_member(
                g,
                &obj_id,
                &schema.name,
                &obj.name,
                &trg.name,
                VertexKind::Trigger,
            ) else {
                continue;
            };
            // 발화 대상은 카탈로그가 알려준 사실 — 몸체 파싱 없이도 fires를 둘 수 있다.
            g.add_edge(Edge {
                from: trg_id,
                to: obj_id.clone(),
                kind: EdgeKind::Fires,
                evidence: vec![Evidence {
                    layer: EvidenceLayer::Catalog,
                    detail: format!("trigger {} on {}", trg.name, obj.name),
                }],
            });
        }
    }

    // 멤버는 두 번째 패스에서 만든다 — 정렬상 멤버가 부모 패키지보다
    // 먼저 오면 find_package가 실패해 스키마 직속으로 잘못 귀속된다.
    for routine in schema
        .routines
        .iter()
        .filter(|r| r.member_of.is_none())
        .chain(schema.routines.iter().filter(|r| r.member_of.is_some()))
    {
        let kind = match routine.kind.as_str() {
            "procedure" => VertexKind::Procedure,
            "package" => VertexKind::Package,
            "query" => VertexKind::Query,
            _ => VertexKind::Function,
        };
        // 함수와 프로시저는 같은 이름을 공유할 수 있다(MySQL) — member와
        // 같은 규칙으로 충돌을 분리해 나중 정점이 조용히 드랍되지 않게 한다.
        let suffix = match kind {
            VertexKind::Procedure => "procedure",
            VertexKind::Package => "package",
            VertexKind::Query => "query",
            _ => "function",
        };
        if let Some(pkg) = &routine.member_of {
            // 패키지 멤버 — schema.pkg.member id로 패키지 아래에 둔다.
            // 스키마 직속 contains 대신 패키지→멤버 contains를 만든다.
            let Some(mem_id) = resolve_collision(
                g,
                VertexId::routine(
                    &schema.name,
                    Some(pkg),
                    &routine.name,
                    routine.signature.as_deref(),
                ),
                &routine.name,
                kind,
                suffix,
                "routine",
            ) else {
                continue;
            };
            g.add_vertex(Vertex {
                id: mem_id.clone(),
                kind,
                name: routine.name.clone(),
                schema: schema.name.clone(),
            });
            packages.register(g, &mem_id, kind);
            if let Some(u) = &routine.usage {
                g.set_usage(mem_id.clone(), to_usage(u));
            }
            match find_package(g, packages, &schema.name, pkg) {
                Some(pkg_id) => g.add_edge(contains(&pkg_id, &mem_id)),
                // 부모 패키지가 수확 안 됐으면 멤버를 버리지 않고 스키마
                // 직속으로 둔다 — 고립보다 귀속 없음이 덜 거짓이다.
                None => {
                    g.add_edge(contains(&schema_id, &mem_id));
                    g.add_limitation(format!(
                        "{}.{pkg}.{}: 부모 패키지 정점이 카탈로그에 없음 \
                         — 스키마 직속으로 둠",
                        schema.name, routine.name
                    ));
                }
            }
            continue;
        }
        let Some(rt_id) = resolve_collision(
            g,
            VertexId::routine(
                &schema.name,
                None,
                &routine.name,
                routine.signature.as_deref(),
            ),
            &routine.name,
            kind,
            suffix,
            "routine",
        ) else {
            continue;
        };
        g.add_vertex(Vertex {
            id: rt_id.clone(),
            kind,
            name: routine.name.clone(),
            schema: schema.name.clone(),
        });
        packages.register(g, &rt_id, kind);
        if let Some(u) = &routine.usage {
            g.set_usage(rt_id.clone(), to_usage(u));
        }
        g.add_edge(contains(&schema_id, &rt_id));
    }
}

/// 스키마 안의 패키지 정점을 찾는다 — 정확 id 우선, `@package` 분리본도 본다.
fn find_package(g: &Graph, packages: &PackageIndex, schema: &str, pkg: &str) -> Option<VertexId> {
    let exact = VertexId::object(schema, pkg);
    if g.vertex(&exact)
        .map(|v| v.kind == VertexKind::Package)
        .unwrap_or(false)
    {
        return Some(exact);
    }
    packages.find(schema, pkg)
}

/// 스키마·이름별 패키지 정점 중 id 순으로 첫 정점.
///
/// 정확한 id로 부모 패키지를 못 찾은 멤버(없는 패키지, `@package`로 분리된 패키지)마다
/// 정점 전체를 훑으면 멤버 × 정점이 된다. 결과를 (스키마, 패키지)별로 memo하면 서로 다른
/// 없는 패키지를 가리키는 멤버들은 여전히 훑고, 멤버 패스 중에 생기는 패키지(패키지인
/// 멤버)를 못 본다. 패키지 정점은 routine으로만 생기므로 routine 정점을 넣을 때마다
/// 등록해, id 순 첫 패키지를 고르던 선형 탐색과 늘 같은 답을 낸다.
#[derive(Default)]
struct PackageIndex(BTreeMap<String, BTreeMap<String, VertexId>>);

impl PackageIndex {
    /// 방금 넣은 `kind` routine 정점 `id`가 패키지면 그래프에 있는 그 정점을 등록한다 —
    /// 같은 이름이면 작은 id가 남는다. 대부분인 패키지 아닌 routine은 정점을 찾지 않는다.
    fn register(&mut self, g: &Graph, id: &VertexId, kind: VertexKind) {
        if kind != VertexKind::Package {
            return;
        }
        let Some(vertex) = g.vertex(id).filter(|v| v.kind == VertexKind::Package) else {
            return;
        };
        let by_name = self.0.entry(vertex.schema.clone()).or_default();
        let first = by_name
            .entry(vertex.name.clone())
            .or_insert_with(|| vertex.id.clone());
        if vertex.id < *first {
            *first = vertex.id.clone();
        }
    }

    /// 스키마 `schema`에서 이름이 `pkg`인 패키지 중 id 순으로 첫 정점.
    fn find(&self, schema: &str, pkg: &str) -> Option<VertexId> {
        self.0.get(schema)?.get(pkg).cloned()
    }
}

/// 스키마의 fk 제약을 object 레벨 + column 레벨 간선으로 풀어 `edges`에 모은다.
fn collect_foreign_key_edges(g: &mut Graph, schema: &SchemaDoc, edges: &mut Vec<Edge>) {
    for object in &schema.objects {
        let object_id = VertexId::object(&schema.name, &object.name);
        for constraint in object
            .constraints
            .iter()
            .filter(|constraint| constraint.kind == "fk")
        {
            build_fk_edges(g, schema, object, constraint, &object_id, edges);
        }
    }
}

/// fk 제약 하나의 간선을 `edges`에 더한다 — 대상 테이블이 없으면 limitation만 남긴다.
fn build_fk_edges(
    g: &mut Graph,
    schema: &SchemaDoc,
    obj: &ObjectDoc,
    con: &ConstraintDoc,
    obj_id: &VertexId,
    edges: &mut Vec<Edge>,
) {
    let Some(referenced) = &con.referenced else {
        return;
    };
    // 대상 스키마가 카탈로그에 없으면 같은 스키마로 해석한다 — SQLite처럼
    // 스키마가 하나뿐인 DB에서 사실상 항상 맞고, 틀릴 경우는 limitations에
    // 남길 수 있는 게 아니라 조용히 다른 정점을 가리키게 되므로 이름이
    // 존재하는지 검사해 없으면 limitation을 남긴다.
    let target_schema = referenced
        .schema
        .clone()
        .unwrap_or_else(|| schema.name.clone());
    let target_id = VertexId::object(&target_schema, &referenced.table);
    if g.vertex(&target_id).is_none() {
        g.add_limitation(format!(
            "foreign key {}.{} -> {} could not be resolved to a collected table; no edge inferred",
            schema.name, obj.name, target_id
        ));
        return;
    }

    edges.push(Edge {
        from: obj_id.clone(),
        to: target_id.clone(),
        kind: EdgeKind::References,
        evidence: vec![Evidence {
            layer: EvidenceLayer::Catalog,
            detail: format!(
                "fk {}: {}.{}({}) -> {}.{}({})",
                con.name,
                schema.name,
                obj.name,
                con.columns.join(","),
                target_schema,
                referenced.table,
                referenced.columns.join(",")
            ),
        }],
    });

    for (local, remote) in con.columns.iter().zip(referenced.columns.iter()) {
        if remote.is_empty() {
            // 대상 컬럼을 카탈로그가 안 알려주면(암시적 PK 참조) 컬럼 간선을
            // 만들지 않는다 — object 간선은 이미 있다.
            continue;
        }
        let Some(local_id) = member_id(g, obj_id, local, VertexKind::Column) else {
            continue;
        };
        let Some(remote_id) = member_id(g, &target_id, remote, VertexKind::Column) else {
            continue;
        };
        edges.push(Edge {
            from: local_id,
            to: remote_id,
            kind: EdgeKind::References,
            evidence: vec![Evidence {
                layer: EvidenceLayer::Catalog,
                detail: format!("fk {}: {} -> {}", con.name, local, remote),
            }],
        });
    }
}

fn contains(from: &VertexId, to: &VertexId) -> Edge {
    Edge {
        from: from.clone(),
        to: to.clone(),
        kind: EdgeKind::Contains,
        evidence: vec![],
    }
}

/// 멤버 정점을 추가한다 — 충돌 시 `@kind` 접미사로 분리한다.
///
/// 컬럼·제약·인덱스·트리거는 `schema.object.name` 공간을 공유하는데,
/// MySQL의 FK 자동 인덱스처럼 다른 kind가 같은 이름을 쓸 수 있다.
/// add_vertex는 first-wins라 나중 정점이 조용히 드랍되므로, 다른 kind가
/// 이미 차지한 이름은 `name@kind`로 분리해 정점을 살린다 — `@`는 SQL
/// 식별자에 못 쓰는 문자라 추가 충돌이 거의 없다. 실제로 쓰인 id를
/// 돌려주고, 분리조차 실패하면 None(정점 생략 + limitation 신고).
fn add_member(
    g: &mut Graph,
    obj_id: &VertexId,
    schema: &str,
    obj: &str,
    name: &str,
    kind: VertexKind,
) -> Option<VertexId> {
    let base = VertexId::member(schema, obj, name);
    let suffix = match kind {
        VertexKind::Column => "column",
        VertexKind::Constraint => "constraint",
        VertexKind::Index => "index",
        VertexKind::Trigger => "trigger",
        _ => "member",
    };
    let id = resolve_collision(g, base, name, kind, suffix, "멤버")?;
    g.add_vertex(Vertex {
        id: id.clone(),
        kind,
        name: name.to_owned(),
        schema: schema.to_owned(),
    });
    g.add_edge(contains(obj_id, &id));
    Some(id)
}

/// id가 이미 점유됐을 때의 분리 규칙 — member·routine 정점이 공유한다.
/// 같은 kind면 진짜 중복이라 합치고, 다른 kind면 `name@kind`로 옮긴다.
/// 반환 id가 실제 정점 id — 호출자는 이 id로 정점·간선·usage를 달아야 한다.
fn resolve_collision(
    g: &mut Graph,
    base: VertexId,
    name: &str,
    kind: VertexKind,
    suffix: &str,
    label: &str,
) -> Option<VertexId> {
    match g.vertex(&base) {
        None => Some(base),
        Some(v) if v.kind == kind => {
            // 같은 kind의 같은 이름은 진짜 중복 — 정점은 합쳐지지만 신고한다.
            g.add_limitation(format!(
                "{label} 이름 중복: {}는 {suffix}가 이미 있다 — 정점이 합쳐진다",
                base.as_str(),
            ));
            Some(base)
        }
        Some(_) => {
            // `@`는 SQL 식별자에 못 쓰는 문자라 base 뒤에 붙여도 안전하다 —
            // member/schema.object 어느 깊이든 같은 규칙으로 분리한다.
            let renamed = VertexId::from_raw(&format!("{}@{suffix}", base.as_str()));
            if g.vertex(&renamed).is_some() {
                g.add_limitation(format!(
                    "{label} id 충돌 미해소: {}와 {} 모두 점유 — {name} {suffix} 정점 생략",
                    base.as_str(),
                    renamed.as_str(),
                ));
                return None;
            }
            g.add_limitation(format!(
                "{label} id 충돌: {}는 다른 kind가 먼저 차지 — {suffix} 정점은 {}로 분리",
                base.as_str(),
                renamed.as_str(),
            ));
            Some(renamed)
        }
    }
}

/// UsageDoc → core Usage — 와이어 타입과 도메인 타입을 분리해 둔 변환.
fn to_usage(u: &UsageDoc) -> Usage {
    Usage {
        since: u.since.clone(),
        reads: u.reads,
        writes: u.writes,
        scans: u.scans,
        total_ms: u.total_ms,
        self_ms: u.self_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_fk() -> CatalogDocument {
        CatalogDocument {
            context: None,
            dependencies: Vec::new(),
            version: 1,
            dialect: "sqlite".into(),
            reader: "test".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "main".into(),
                routines: vec![],
                objects: vec![
                    ObjectDoc {
                        name: "customers".into(),
                        kind: "table".into(),
                        columns: vec![ColumnDoc {
                            name: "id".into(),
                            data_type: "INTEGER".into(),
                            nullable: false,
                            default: None,
                            ordinal: 1,
                            pk_position: 1,
                        }],
                        constraints: vec![],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                    ObjectDoc {
                        name: "orders".into(),
                        kind: "table".into(),
                        columns: vec![ColumnDoc {
                            name: "customer_id".into(),
                            data_type: "INTEGER".into(),
                            nullable: false,
                            default: None,
                            ordinal: 1,
                            pk_position: 0,
                        }],
                        constraints: vec![ConstraintDoc {
                            name: "orders_fk_0".into(),
                            kind: "fk".into(),
                            columns: vec!["customer_id".into()],
                            referenced: Some(ReferencedDoc {
                                schema: None,
                                table: "customers".into(),
                                columns: vec!["id".into()],
                            }),
                        }],
                        indexes: vec![],
                        triggers: vec![],
                        body: None,
                        usage: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn fk는_object와_column_두_레벨의_간선이_된다() {
        let g = document_to_graph(&doc_with_fk());
        let edges = g.edges();
        // object 레벨
        assert!(edges.iter().any(|e| e.kind == EdgeKind::References
            && e.from.as_str() == "main.orders"
            && e.to.as_str() == "main.customers"));
        // column 레벨
        assert!(edges.iter().any(|e| e.kind == EdgeKind::References
            && e.from.as_str() == "main.orders.customer_id"
            && e.to.as_str() == "main.customers.id"));
    }

    #[test]
    fn contains는_의존성이_아니다() {
        let g = document_to_graph(&doc_with_fk());
        let orders = VertexId::object("main", "orders");
        let dep: Vec<_> = g
            .outgoing(&orders)
            .iter()
            .filter(|e| e.kind.is_dependency())
            .collect();
        // orders의 의존 간선은 customers를 향한 references 하나뿐.
        assert_eq!(dep.len(), 1);
        assert_eq!(dep[0].to.as_str(), "main.customers");
    }

    #[test]
    fn 수집되지_않은_fk_대상에는_유령_간선을_만들지_않는다() {
        let mut doc = doc_with_fk();
        doc.schemas[0].objects[1].constraints[0]
            .referenced
            .as_mut()
            .unwrap()
            .table = "missing_customers".into();
        let graph = document_to_graph(&doc);
        assert!(graph
            .vertex(&VertexId::object("main", "missing_customers"))
            .is_none());
        assert!(!graph.edges().iter().any(|edge| {
            edge.from.as_str() == "main.orders" && edge.to.as_str() == "main.missing_customers"
        }));
        assert!(graph
            .limitations()
            .iter()
            .any(|note| note.contains("no edge inferred")));
    }

    #[test]
    fn 명시된_schema_metadata는_실제_컬럼_index_fk_id를_가리킨다() {
        let mut doc = doc_with_fk();
        doc.context = Some(CollectionContext {
            source_id: "test".into(),
            database: None,
            schema_filter: None,
            catalog_complete: true,
        });
        doc.schemas[0].objects[1].indexes.push(IndexDoc {
            has_predicate: None,
            definition_complete: Some(true),
            predicate: None,
            name: "orders_customer_idx".into(),
            unique: false,
            columns: vec!["customer_id".into()],
            usage: None,
        });
        let graph = document_to_graph(&doc);
        let metadata = graph.schema_metadata().expect("schema metadata");
        assert!(metadata
            .columns
            .contains_key(&VertexId::member("main", "orders", "customer_id")));
        assert_eq!(
            metadata.columns[&VertexId::member("main", "customers", "id")].pk_position,
            1
        );
        let index_id = VertexId::member("main", "orders", "orders_customer_idx");
        assert_eq!(
            metadata.indexes[&index_id].columns,
            vec![VertexId::member("main", "orders", "customer_id")]
        );
        let fk_id = VertexId::member("main", "orders", "orders_fk_0");
        assert!(metadata.foreign_keys[&fk_id].complete);
        assert!(metadata.catalog_complete);
        doc.context.as_mut().unwrap().catalog_complete = false;
        let incomplete = document_to_graph(&doc);
        let incomplete = incomplete.schema_metadata().unwrap();
        assert!(!incomplete.foreign_keys[&fk_id].complete);
        // PK 부재를 확정할 근거가 사라진다.
        assert!(!incomplete.catalog_complete);
    }

    /// 옛 PostgreSQL 수집기처럼 pk_position을 빠뜨린 문서도 PK 제약 순서로 보정한다.
    #[test]
    fn pk_position이_없으면_pk_제약의_키_순서로_보정한다() {
        let mut doc = doc_with_fk();
        let customers = doc.schemas[0]
            .objects
            .iter_mut()
            .find(|object| object.name == "customers")
            .expect("fixture has customers");
        for column in &mut customers.columns {
            column.pk_position = 0;
        }
        customers.constraints.push(ConstraintDoc {
            name: "customers_pkey".into(),
            kind: "pk".into(),
            columns: vec!["id".into()],
            referenced: None,
        });
        doc.context = Some(CollectionContext {
            source_id: "test".into(),
            database: None,
            schema_filter: None,
            catalog_complete: true,
        });
        let graph = document_to_graph(&doc);
        let metadata = graph.schema_metadata().expect("schema metadata");
        assert_eq!(
            metadata.columns[&VertexId::member("main", "customers", "id")].pk_position,
            1
        );
    }

    /// `REFERENCES customers`처럼 대상 컬럼을 생략한 FK는 대상 PK로 해석해 완전한 FK가 된다.
    #[test]
    fn 대상_컬럼을_생략한_fk는_대상_pk로_해석한다() {
        let mut doc = doc_with_fk();
        doc.context = Some(CollectionContext {
            source_id: "test".into(),
            database: None,
            schema_filter: None,
            catalog_complete: true,
        });
        for object in &mut doc.schemas[0].objects {
            for constraint in &mut object.constraints {
                if let Some(reference) = constraint.referenced.as_mut() {
                    reference.columns = vec![String::new()];
                }
            }
        }
        let graph = document_to_graph(&doc);
        let metadata = graph.schema_metadata().expect("schema metadata");
        let fk = &metadata.foreign_keys[&VertexId::member("main", "orders", "orders_fk_0")];
        assert_eq!(
            fk.target_columns,
            vec![VertexId::member("main", "customers", "id")]
        );
        assert!(fk.complete);
    }

    #[test]
    fn document의_usage는_정점에_매핑되고_없음과_0은_다르다() {
        let mut doc = doc_with_fk();
        // orders: 0 관측(usage가 있는 채로 0) — 미수집과 구분해야 한다.
        doc.schemas[0].objects[1].usage = Some(UsageDoc {
            since: None,
            reads: 0,
            writes: 0,
            scans: None,
            total_ms: None,
            self_ms: None,
        });
        // customers의 인덱스에도 usage를 둔다(인덱스는 멤버 레벨 정점).
        doc.schemas[0].objects[0].indexes.push(IndexDoc {
            has_predicate: None,
            definition_complete: None,
            predicate: None,
            name: "idx_id".into(),
            columns: vec!["id".into()],
            unique: true,
            usage: Some(UsageDoc {
                since: Some("2025-01-01".into()),
                reads: 7,
                writes: 0,
                scans: None,
                total_ms: None,
                self_ms: None,
            }),
        });

        let g = document_to_graph(&doc);
        let orders = VertexId::object("main", "orders");
        let usage = g.usage(&orders).expect("orders usage");
        assert_eq!(usage.reads, 0);
        assert_eq!(usage.since, None);
        let idx = VertexId::member("main", "customers", "idx_id");
        assert_eq!(g.usage(&idx).map(|u| u.reads), Some(7));
        // customers 객체 자체는 미수집 — None이어야 0 관측과 구분된다.
        assert!(g.usage(&VertexId::object("main", "customers")).is_none());
    }

    #[test]
    fn 멤버_id_충돌은_kind_접미사로_분리된다() {
        let mut doc = doc_with_fk();
        // MySQL의 FK 자동 인덱스는 컬럼 이름을 그대로 쓴다 — 컬럼과 충돌.
        doc.schemas[0].objects[1].indexes.push(IndexDoc {
            has_predicate: None,
            definition_complete: None,
            predicate: None,
            name: "customer_id".into(),
            columns: vec!["customer_id".into()],
            unique: false,
            usage: Some(UsageDoc {
                since: Some("2025-01-01".into()),
                reads: 4,
                writes: 0,
                scans: None,
                total_ms: None,
                self_ms: None,
            }),
        });
        // 제약과도 충돌하는 인덱스 — fk 제약과 같은 이름.
        doc.schemas[0].objects[1].indexes.push(IndexDoc {
            has_predicate: None,
            definition_complete: None,
            predicate: None,
            name: "orders_fk_0".into(),
            columns: vec!["customer_id".into()],
            unique: false,
            usage: None,
        });

        let g = document_to_graph(&doc);
        // 컬럼은 원래 id를 지키고, 인덱스는 @index로 분리된다.
        assert!(g
            .vertex(&VertexId::member("main", "orders", "customer_id"))
            .is_some());
        let idx_id = VertexId::from_raw("main.orders.customer_id@index");
        let idx = g.vertex(&idx_id).expect("분리된 index 정점");
        assert_eq!(idx.kind, VertexKind::Index);
        // usage도 분리된 정점에 붙는다.
        assert_eq!(g.usage(&idx_id).map(|u| u.reads), Some(4));
        // 제약 이름과 충돌한 인덱스도 분리된다.
        assert_eq!(
            g.vertex(&VertexId::from_raw("main.orders.orders_fk_0@index"))
                .map(|v| v.kind),
            Some(VertexKind::Index)
        );
        // 충돌은 limitation으로 신고된다.
        assert!(g.limitations().iter().any(|l| l.contains("멤버 id 충돌")));
    }

    #[test]
    fn routine_이름_충돌과_usage가_분리된_정점에_붙는다() {
        let mut doc = doc_with_fk();
        // 테이블과 같은 이름의 함수 — MySQL에선 함수/프로시저가 이름을 공유할 수 있다.
        doc.schemas[0].routines = vec![
            RoutineDoc {
                source: None,
                name: "orders".into(), // 테이블 orders와 충돌
                kind: "function".into(),
                language: Some("sql".into()),
                body: None,
                signature: None,
                usage: Some(UsageDoc {
                    since: Some("2025-01-01".into()),
                    reads: 12,
                    writes: 0,
                    scans: None,
                    total_ms: Some(120.5),
                    self_ms: Some(80.0),
                }),
                member_of: None,
            },
            RoutineDoc {
                source: None,
                name: "helper".into(),
                kind: "procedure".into(),
                language: Some("sql".into()),
                body: None,
                signature: None,
                usage: Some(UsageDoc {
                    since: None,
                    reads: 3,
                    writes: 0,
                    scans: None,
                    total_ms: None,
                    self_ms: None,
                }),
                member_of: None,
            },
        ];

        let g = document_to_graph(&doc);
        // 테이블이 base id를 지키고 함수는 @function으로 분리된다.
        assert_eq!(
            g.vertex(&VertexId::object("main", "orders"))
                .map(|v| v.kind),
            Some(VertexKind::Table)
        );
        let fn_id = VertexId::from_raw("main.orders@function");
        assert_eq!(g.vertex(&fn_id).map(|v| v.kind), Some(VertexKind::Function));
        // usage는 분리된 정점에 붙는다 — 시간 필드도 함께 간다.
        let fu = g.usage(&fn_id).unwrap();
        assert_eq!(fu.reads, 12);
        assert_eq!((fu.total_ms, fu.self_ms), (Some(120.5), Some(80.0)));
        assert_eq!(
            g.usage(&VertexId::object("main", "helper"))
                .map(|u| u.reads),
            Some(3)
        );
        assert!(g
            .limitations()
            .iter()
            .any(|l| l.contains("routine id 충돌")));
    }

    #[test]
    fn 패키지_멤버가_문서에서_패키지보다_먼저_와도_귀속된다() {
        // 정렬상 멤버 이름이 패키지보다 앞설 때 — 패키지 정점이 아직 없는
        // 시점에 멤버를 만들면 스키마 직속으로 잘못 떨어진다(실검증 발견).
        let mut doc = doc_with_fk();
        doc.schemas[0].routines = vec![
            RoutineDoc {
                source: None,
                name: "aaa_touch".into(), // zzz_ops보다 정렬이 빠르다
                kind: "procedure".into(),
                language: Some("plsql".into()),
                body: None,
                signature: None,
                usage: None,
                member_of: Some("zzz_ops".into()),
            },
            RoutineDoc {
                source: None,
                name: "zzz_ops".into(),
                kind: "package".into(),
                language: Some("plsql".into()),
                body: None,
                signature: None,
                usage: None,
                member_of: None,
            },
        ];

        let g = document_to_graph(&doc);
        let mem = VertexId::member("main", "zzz_ops", "aaa_touch");
        assert_eq!(g.vertex(&mem).map(|v| v.kind), Some(VertexKind::Procedure));
        assert!(g.edges().iter().any(|e| e.kind == EdgeKind::Contains
            && e.from == VertexId::object("main", "zzz_ops")
            && e.to == mem));
        assert!(!g
            .limitations()
            .iter()
            .any(|l| l.contains("부모 패키지 정점이 카탈로그에 없음")));
    }

    /// 패키지 `package`의 멤버 프로시저 `name`.
    fn package_member(name: &str, package: &str) -> RoutineDoc {
        RoutineDoc {
            source: None,
            name: name.into(),
            kind: "procedure".into(),
            language: Some("plsql".into()),
            body: None,
            signature: None,
            usage: None,
            member_of: Some(package.into()),
        }
    }

    /// 패키지 routine — `member_of`가 있으면 다른 패키지의 멤버로 들어간다.
    fn package(name: &str, member_of: Option<&str>) -> RoutineDoc {
        RoutineDoc {
            kind: "package".into(),
            member_of: member_of.map(Into::into),
            ..package_member(name, "")
        }
    }

    /// 정점으로 들어오는 Contains 간선의 부모 id들.
    fn contains_parents(g: &Graph, id: &VertexId) -> Vec<String> {
        g.incoming(id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Contains)
            .map(|edge| edge.from.to_string())
            .collect()
    }

    /// 부모 패키지를 못 찾아 스키마 직속으로 둔 멤버 limitation 수.
    fn orphan_member_notes(g: &Graph) -> usize {
        g.limitations()
            .iter()
            .filter(|l| l.contains("부모 패키지 정점이 카탈로그에 없음"))
            .count()
    }

    #[test]
    fn 부모_패키지가_없는_멤버는_멤버마다_스키마_직속으로_남는다() {
        let mut doc = doc_with_fk();
        let members = [("m1", "ghost"), ("m2", "ghost"), ("m3", "other_ghost")];
        doc.schemas[0].routines = members
            .iter()
            .map(|(name, package)| package_member(name, package))
            .collect();
        let g = document_to_graph(&doc);
        for (name, package) in members {
            let id = VertexId::member("main", package, name);
            assert_eq!(contains_parents(&g, &id), ["main"], "{id}");
        }
        assert_eq!(orphan_member_notes(&g), members.len());
    }

    #[test]
    fn 이름_충돌로_분리된_패키지도_멤버의_부모가_된다() {
        let mut doc = doc_with_fk();
        // 테이블 orders가 base id를 차지해 패키지 orders는 orders@package로 분리된다.
        doc.schemas[0].routines = vec![package_member("run", "orders"), package("orders", None)];
        let g = document_to_graph(&doc);
        let member = VertexId::member("main", "orders", "run");
        assert_eq!(contains_parents(&g, &member), ["main.orders@package"]);
        assert_eq!(orphan_member_notes(&g), 0);
    }

    #[test]
    fn 패키지인_멤버는_만들어진_뒤의_멤버부터_부모가_된다() {
        let mut doc = doc_with_fk();
        // 멤버 패스는 문서 순서다. 패키지 inner가 생기기 전의 early는 귀속처가 없고,
        // 그 뒤의 late는 outer 아래에 생긴 inner를 이름으로 찾는다.
        doc.schemas[0].routines = vec![
            package_member("early", "inner"),
            package("inner", Some("outer")),
            package_member("late", "inner"),
            package("outer", None),
        ];
        let g = document_to_graph(&doc);
        let inner = VertexId::member("main", "outer", "inner");
        assert_eq!(g.vertex(&inner).map(|v| v.kind), Some(VertexKind::Package));
        assert_eq!(contains_parents(&g, &inner), ["main.outer"]);
        let early = VertexId::member("main", "inner", "early");
        assert_eq!(contains_parents(&g, &early), ["main"]);
        let late = VertexId::member("main", "inner", "late");
        assert_eq!(contains_parents(&g, &late), ["main.outer.inner"]);
        assert_eq!(orphan_member_notes(&g), 1);
    }

    /// `columns` 컬럼과 `constraints` 제약을 가진 테이블.
    fn table_doc(name: &str, columns: &[&str], constraints: Vec<ConstraintDoc>) -> ObjectDoc {
        let mut object = doc_with_fk().schemas[0].objects[0].clone();
        let column = object.columns[0].clone();
        object.name = name.into();
        object.columns = columns
            .iter()
            .map(|name| ColumnDoc {
                name: (*name).into(),
                ..column.clone()
            })
            .collect();
        object.constraints = constraints;
        object
    }

    /// `columns` → `target`(`target_columns`) FK 제약. `schema`가 None이면 같은 스키마다.
    fn fk_doc(
        name: &str,
        columns: &[&str],
        schema: Option<&str>,
        target: &str,
        target_columns: &[&str],
    ) -> ConstraintDoc {
        let names = |items: &[&str]| -> Vec<String> {
            items.iter().map(|item| (*item).to_string()).collect()
        };
        ConstraintDoc {
            name: name.into(),
            kind: "fk".into(),
            columns: names(columns),
            referenced: Some(ReferencedDoc {
                schema: schema.map(Into::into),
                table: target.into(),
                columns: names(target_columns),
            }),
        }
    }

    /// from → to References 간선의 근거 detail들.
    fn reference_details(g: &Graph, from: &str, to: &str) -> Vec<String> {
        let edge = g.edges().into_iter().find(|e| {
            e.kind == EdgeKind::References && e.from.as_str() == from && e.to.as_str() == to
        });
        let edge = edge.unwrap_or_else(|| panic!("references {from} -> {to}"));
        edge.evidence.iter().map(|e| e.detail.clone()).collect()
    }

    #[test]
    fn 같은_테이블_쌍의_fk_근거는_정렬돼_간선_하나에_모인다() {
        let mut doc = doc_with_fk();
        // 이름이 내림차순으로 와서 근거를 하나씩 합치면 매번 기존 근거 앞에 들어간다.
        let fk = |name: &str| fk_doc(name, &["customer_id"], None, "customers", &["id"]);
        doc.schemas[0].objects[1]
            .constraints
            .extend([fk("orders_fk_z"), fk("orders_fk_m")]);
        let g = document_to_graph(&doc);
        let object_detail =
            |name: &str| format!("fk {name}: main.orders(customer_id) -> main.customers(id)");
        assert_eq!(
            reference_details(&g, "main.orders", "main.customers"),
            ["orders_fk_0", "orders_fk_m", "orders_fk_z"].map(object_detail)
        );
        assert_eq!(
            reference_details(&g, "main.orders.customer_id", "main.customers.id"),
            ["orders_fk_0", "orders_fk_m", "orders_fk_z"]
                .map(|name| format!("fk {name}: customer_id -> id"))
        );
        // FK 간선은 테이블의 소유 간선(컬럼 하나, 제약 셋) 뒤에 한 번만 붙고, in/out 근거가 같다.
        let orders = VertexId::object("main", "orders");
        let kinds: Vec<_> = g.outgoing(&orders).iter().map(|e| e.kind).collect();
        use EdgeKind::{Contains, References};
        assert_eq!(kinds, [Contains, Contains, Contains, Contains, References]);
        let customers = VertexId::object("main", "customers");
        assert_eq!(
            g.incoming(&customers).last().map(|e| &e.evidence),
            g.outgoing(&orders).last().map(|e| &e.evidence)
        );
    }

    #[test]
    fn fk_간선은_스키마를_건너도_문서_순서대로_붙는다() {
        let mut doc = doc_with_fk();
        // 스키마 z가 a보다 먼저 온다. a.t를 z.v와 a.u가 참조하고 z.v는 같은 FK가 둘이다.
        let to_t = |name: &str| fk_doc(name, &["t_id"], Some("a"), "t", &["id"]);
        let z_v = table_doc("v", &["t_id"], vec![to_t("v_fk_2"), to_t("v_fk_1")]);
        let a_u = table_doc("u", &["t_id"], vec![to_t("u_fk")]);
        let a_t = table_doc("t", &["id"], vec![]);
        doc.schemas = vec![
            SchemaDoc {
                name: "z".into(),
                objects: vec![z_v],
                routines: vec![],
            },
            SchemaDoc {
                name: "a".into(),
                objects: vec![a_t, a_u],
                routines: vec![],
            },
        ];
        let g = document_to_graph(&doc);
        let sources = |id: &VertexId| -> Vec<(String, EdgeKind)> {
            let edges = g.incoming(id).iter();
            edges.map(|e| (e.from.to_string(), e.kind)).collect()
        };
        let reference = |from: &str| (from.to_string(), EdgeKind::References);
        let contains = |from: &str| (from.to_string(), EdgeKind::Contains);
        assert_eq!(
            sources(&VertexId::object("a", "t")),
            [contains("a"), reference("z.v"), reference("a.u")]
        );
        assert_eq!(
            sources(&VertexId::member("a", "t", "id")),
            [
                contains("a.t"),
                reference("z.v.t_id"),
                reference("a.u.t_id")
            ]
        );
        assert_eq!(
            reference_details(&g, "z.v", "a.t"),
            ["v_fk_1", "v_fk_2"].map(|name| format!("fk {name}: z.v(t_id) -> a.t(id)"))
        );
    }
}
