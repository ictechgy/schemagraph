//! SQL 스코프와 값 계보를 필요한 간선·금지된 간선 양쪽으로 검증한다.

use schemagraph_core::{AnalysisState, EdgeKind, Graph, VertexId};
use schemagraph_source::document::{
    CatalogDocument, CollectionContext, ColumnDoc, ObjectDoc, RoutineDoc, SchemaDoc, TriggerDoc,
};

fn columns(names: &[&str]) -> Vec<ColumnDoc> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| ColumnDoc {
            name: (*name).into(),
            data_type: "INTEGER".into(),
            nullable: true,
            default: None,
            ordinal: i as u32 + 1,
            pk_position: 0,
        })
        .collect()
}

fn object(name: &str, names: &[&str]) -> ObjectDoc {
    ObjectDoc {
        name: name.into(),
        kind: "table".into(),
        columns: columns(names),
        constraints: vec![],
        indexes: vec![],
        triggers: vec![],
        body: None,
        usage: None,
    }
}

fn scan(sql: &str, outputs: &[&str]) -> Graph {
    scan_with_dialect(sql, outputs, "postgres")
}

fn scan_with_dialect(sql: &str, outputs: &[&str], dialect: &str) -> Graph {
    let mut view = object("v", outputs);
    view.kind = "view".into();
    view.body = Some(sql.into());
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            routines: vec![],
            objects: vec![
                object("orders", &["id", "customer_id", "amount"]),
                object("customers", &["id", "name"]),
                view,
            ],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    for edge in graph.edges() {
        assert!(graph.vertex(&edge.from).is_some());
        assert!(graph.vertex(&edge.to).is_some());
    }
    graph
}

fn edge(g: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    g.edges()
        .iter()
        .any(|e| e.from.as_str() == from && e.to.as_str() == to && e.kind == kind)
}

fn complete(g: &Graph) {
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Complete,
        "{:?}",
        g.limitations()
    );
}

fn has_diagnostic(g: &Graph, code: &str) -> bool {
    g.analysis()
        .values()
        .flat_map(|analysis| &analysis.diagnostics)
        .any(|diagnostic| diagnostic.code == code)
}

fn scan_routine(body: &str) -> Graph {
    scan_routine_with(body, "postgres", "sql")
}

fn scan_routine_with(body: &str, dialect: &str, language: &str) -> Graph {
    scan_routine_kind(body, dialect, language, "procedure")
}

fn scan_routine_kind(body: &str, dialect: &str, language: &str, kind: &str) -> Graph {
    scan_routine_in_context(body, dialect, language, kind, None)
}

/// `scan_routine_kind`와 같은 카탈로그에 수집 context(SQL Server collation 등)를 싣는다.
fn scan_routine_in_context(
    body: &str,
    dialect: &str,
    language: &str,
    kind: &str,
    context: Option<CollectionContext>,
) -> Graph {
    scan_routine_with_objects(body, dialect, language, kind, context, Vec::new())
}

/// `scan_routine_in_context`의 카탈로그에 테이블을 더한다(ASCII 밖 이름의 대상 등).
fn scan_routine_with_objects(
    body: &str,
    dialect: &str,
    language: &str,
    kind: &str,
    context: Option<CollectionContext>,
    extra: Vec<ObjectDoc>,
) -> Graph {
    let mut objects = vec![
        object("orders", &["id", "customer_id", "amount"]),
        object("customers", &["id", "name"]),
        object("audit", &["id", "name"]),
    ];
    objects.extend(extra);
    let doc = CatalogDocument {
        context,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            objects,
            routines: vec![RoutineDoc {
                name: "mutate".into(),
                kind: kind.into(),
                language: Some(language.into()),
                body: Some(body.into()),
                signature: None,
                usage: None,
                member_of: None,
                source: None,
            }],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

#[test]
fn external_select_into_uses_dml_target_binding_instead_of_read_only_query_path() {
    let graph = scan_routine_kind(
        "SELECT c.id, c.name INTO s.audit FROM s.customers c",
        "sqlserver",
        "sql",
        "query",
    );
    assert!(edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes));
    for column in ["id", "name"] {
        assert!(edge(
            &graph,
            "s.mutate",
            &format!("s.audit.{column}"),
            EdgeKind::Writes
        ));
        assert!(edge(
            &graph,
            &format!("s.audit.{column}"),
            &format!("s.customers.{column}"),
            EdgeKind::DerivesFrom
        ));
    }
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Complete
    );
    let read_only = scan_routine_kind(
        "SELECT c.id, c.name FROM s.customers c",
        "sqlserver",
        "sql",
        "query",
    );
    assert!(read_only
        .edges()
        .iter()
        .all(|edge| edge.kind != EdgeKind::Writes));
}

#[test]
fn external_union_select_into_preserves_destination_and_both_value_sources() {
    let graph = scan_routine_kind(
        "SELECT c.id, c.name INTO s.audit FROM s.customers c UNION ALL SELECT o.id, CAST(o.amount AS VARCHAR(30)) FROM s.orders o",
        "sqlserver",
        "sql",
        "query",
    );
    assert!(edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes));
    for (target, sources) in [
        ("s.audit.id", ["s.customers.id", "s.orders.id"]),
        ("s.audit.name", ["s.customers.name", "s.orders.amount"]),
    ] {
        assert!(edge(&graph, "s.mutate", target, EdgeKind::Writes));
        for source in sources {
            assert!(edge(&graph, target, source, EdgeKind::DerivesFrom));
        }
    }
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Complete
    );
}

fn scan_oracle_temp_sequence(body: &str) -> Graph {
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: "oracle".into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "SGACC".into(),
            objects: vec![
                object("DML_SOURCE", &["SOURCE_ID", "AMOUNT", "ACTIVE"]),
                object("DML_UPDATE_DELTA", &["TARGET_ID", "BUMP", "ACTIVE"]),
                object("DML_TARGET", &["TARGET_ID", "TARGET_VALUE", "LAST_WRITER"]),
            ],
            routines: vec![RoutineDoc {
                name: "MUTATE".into(),
                kind: "query".into(),
                language: Some("sql".into()),
                body: Some(body.into()),
                signature: None,
                usage: None,
                member_of: None,
                source: Some("dml/case.sql".into()),
            }],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

fn legacy_scalar_routine_graph(dialect: &str) -> Graph {
    let (schema, genre, genre_id, genre_name, log, observed, rename_body, call_body) = if dialect
        == "oracle"
    {
        (
                "SGACC",
                "GENRE",
                "GENREID",
                "NAME",
                "ACC_GENRE_COUNT_LOG",
                "OBSERVEDCOUNT",
                "PROCEDURE acc_rename_genre(genre_id IN NUMBER, genre_name IN VARCHAR2) AS BEGIN UPDATE Genre SET Name = genre_name WHERE GenreId = genre_id; END;",
                "PROCEDURE acc_call_count AS n NUMBER; BEGIN n := acc_genre_count(); INSERT INTO ACC_GENRE_COUNT_LOG (ObservedCount) VALUES (n); END;",
            )
    } else {
        (
                "dbo",
                "Genre",
                "GenreId",
                "Name",
                "ACC_GENRE_COUNT_LOG",
                "ObservedCount",
                "CREATE PROCEDURE dbo.acc_rename_genre @genre_id INT, @genre_name NVARCHAR(120) AS BEGIN SET NOCOUNT ON; UPDATE Genre SET Name = @genre_name WHERE GenreId = @genre_id; END",
                "CREATE PROCEDURE dbo.acc_call_count AS BEGIN SET NOCOUNT ON; DECLARE @n INT; SET @n = dbo.acc_genre_count(); INSERT INTO ACC_GENRE_COUNT_LOG (ObservedCount) VALUES (@n); END",
            )
    };
    let procedure = |name: &str, body: &str, language: &str| RoutineDoc {
        name: name.into(),
        kind: "procedure".into(),
        language: Some(language.into()),
        body: Some(body.into()),
        signature: None,
        usage: None,
        member_of: None,
        source: None,
    };
    let function_name = if dialect == "oracle" {
        "ACC_GENRE_COUNT"
    } else {
        "acc_genre_count"
    };
    let rename_name = if dialect == "oracle" {
        "ACC_RENAME_GENRE"
    } else {
        "acc_rename_genre"
    };
    let call_name = if dialect == "oracle" {
        "ACC_CALL_COUNT"
    } else {
        "acc_call_count"
    };
    let language = if dialect == "oracle" { "plsql" } else { "sql" };
    let mut routines = vec![
        procedure(rename_name, rename_body, language),
        procedure(call_name, call_body, language),
        RoutineDoc {
            name: function_name.into(),
            kind: "function".into(),
            language: Some(language.into()),
            body: Some(if dialect == "oracle" {
                "FUNCTION acc_genre_count RETURN NUMBER AS n NUMBER; BEGIN SELECT COUNT(GenreId) INTO n FROM Genre; RETURN n; END;".into()
            } else {
                "CREATE FUNCTION dbo.acc_genre_count() RETURNS INT AS BEGIN RETURN (SELECT COUNT(GenreId) FROM Genre); END".into()
            }),
            signature: None,
            usage: None,
            member_of: None,
            source: None,
        },
    ];
    if dialect == "oracle" {
        routines.push(procedure(
            "ACC_SHADOW_NAME",
            "PROCEDURE acc_shadow_name(name IN VARCHAR2) AS BEGIN UPDATE Genre SET Name = Name; END;",
            language,
        ));
    }
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: schema.into(),
            objects: vec![
                object(genre, &[genre_id, genre_name]),
                object(log, &[observed]),
            ],
            routines,
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

#[test]
fn declared_sqlserver_parameters_and_locals_are_not_columns() {
    let graph = legacy_scalar_routine_graph("sqlserver");
    for id in ["dbo.acc_rename_genre", "dbo.acc_call_count"] {
        assert_eq!(
            graph.analysis()[&VertexId::from_raw(id)].state,
            AnalysisState::Complete
        );
        assert!(!graph.analysis()[&VertexId::from_raw(id)]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "SG_COLUMN_UNRESOLVED"));
    }
    assert!(edge(
        &graph,
        "dbo.acc_call_count",
        "dbo.acc_genre_count",
        EdgeKind::Calls
    ));
}

#[test]
fn declared_oracle_parameters_and_locals_are_not_columns() {
    let graph = legacy_scalar_routine_graph("oracle");
    for id in ["SGACC.ACC_RENAME_GENRE", "SGACC.ACC_CALL_COUNT"] {
        assert_eq!(
            graph.analysis()[&VertexId::from_raw(id)].state,
            AnalysisState::Complete
        );
        assert!(!graph.analysis()[&VertexId::from_raw(id)]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "SG_COLUMN_UNRESOLVED"));
    }
    assert!(edge(
        &graph,
        "SGACC.ACC_CALL_COUNT",
        "SGACC.ACC_GENRE_COUNT",
        EdgeKind::Calls
    ));
    let shadow = &graph.analysis()[&VertexId::from_raw("SGACC.ACC_SHADOW_NAME")];
    assert!(shadow
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "SG_TEMPORAL_SELF_LINEAGE"));
    assert!(edge(
        &graph,
        "SGACC.ACC_SHADOW_NAME",
        "SGACC.GENRE.NAME",
        EdgeKind::Reads
    ));
}

#[test]
fn oracle_new_old_fields_bind_to_trigger_parent_columns() {
    let mut genre = object("GENRE", &["GENREID", "NAME"]);
    genre.triggers = vec![
        TriggerDoc {
            name: "ACC_GENRE_INSERT".into(),
            body: Some("BEGIN INSERT INTO ACC_GENRE_AUDIT (GenreId, GenreName) VALUES (:NEW.GenreId, :NEW.Name); END;".into()),
        },
        TriggerDoc {
            name: "ACC_GENRE_UPDATE".into(),
            body: Some("BEGIN INSERT INTO ACC_GENRE_CHANGES (GenreId, OldName, NewName) VALUES (:NEW.GenreId, :OLD.Name, :NEW.Name); END;".into()),
        },
    ];
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: "oracle".into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "SGACC".into(),
            objects: vec![
                genre,
                object("ACC_GENRE_AUDIT", &["GENREID", "GENRENAME"]),
                object("ACC_GENRE_CHANGES", &["GENREID", "OLDNAME", "NEWNAME"]),
            ],
            routines: vec![],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    super::enrich_from_document(&mut graph, &doc);
    for trigger in ["ACC_GENRE_INSERT", "ACC_GENRE_UPDATE"] {
        let id = VertexId::from_raw(&format!("SGACC.GENRE.{trigger}"));
        assert!(!graph.analysis()[&id]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "SG_COLUMN_UNRESOLVED"
                || diagnostic.code == "SG_TEMPORAL_SELF_LINEAGE"));
    }
    assert!(edge(
        &graph,
        "SGACC.ACC_GENRE_AUDIT.GENREID",
        "SGACC.GENRE.GENREID",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "SGACC.ACC_GENRE_CHANGES.OLDNAME",
        "SGACC.GENRE.NAME",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn sqlite_new_field_is_resolved_while_real_self_update_stays_partial() {
    let mut orders = object("orders", &["id", "customer_id"]);
    orders.triggers.push(TriggerDoc {
        name: "trg_orders_touch".into(),
        body: Some("BEGIN UPDATE customers SET name = name WHERE id = NEW.customer_id; END".into()),
    });
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: "sqlite".into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "main".into(),
            objects: vec![orders, object("customers", &["id", "name"])],
            routines: vec![],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    super::enrich_from_document(&mut graph, &doc);
    let analysis = &graph.analysis()[&VertexId::from_raw("main.orders.trg_orders_touch")];
    assert!(analysis
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "SG_TEMPORAL_SELF_LINEAGE"));
    assert!(!analysis
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "SG_COLUMN_UNRESOLVED"));
    assert!(edge(
        &graph,
        "main.orders.trg_orders_touch",
        "main.orders.customer_id",
        EdgeKind::Reads
    ));
}

#[test]
fn oracle_schema_qualified_global_temp_insert_resolves_exact_local_symbol() {
    let graph = scan_oracle_temp_sequence(
        "CREATE GLOBAL TEMPORARY TABLE SGACC.DML_TEMP_STAGE ON COMMIT PRESERVE ROWS AS SELECT s.SOURCE_ID + 400 AS STAGED_ID, s.AMOUNT * 3 AS STAGED_VALUE FROM SGACC.DML_SOURCE s WHERE s.ACTIVE = 1; INSERT INTO SGACC.DML_TARGET (TARGET_ID, TARGET_VALUE, LAST_WRITER) SELECT STAGED_ID, STAGED_VALUE, 'temp_insert' FROM SGACC.DML_TEMP_STAGE",
    );
    assert!(edge(
        &graph,
        "SGACC.DML_TARGET.TARGET_ID",
        "SGACC.DML_SOURCE.SOURCE_ID",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "SGACC.DML_TARGET.TARGET_VALUE",
        "SGACC.DML_SOURCE.AMOUNT",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_diagnostic(&graph, "SG_RELATION_UNRESOLVED"));
    assert!(!graph
        .vertices()
        .any(|vertex| vertex.name == "DML_TEMP_STAGE"));
}

#[test]
fn oracle_schema_qualified_global_temp_update_preserves_temporal_lineage() {
    let graph = scan_oracle_temp_sequence(
        "CREATE GLOBAL TEMPORARY TABLE SGACC.DML_TEMP_DELTA ON COMMIT PRESERVE ROWS AS SELECT TARGET_ID, BUMP + 5 AS BUMP FROM SGACC.DML_UPDATE_DELTA WHERE ACTIVE = 1; UPDATE SGACC.DML_TARGET t SET (TARGET_VALUE, LAST_WRITER) = (SELECT t.TARGET_VALUE + d.BUMP, 'temp_update' FROM SGACC.DML_TEMP_DELTA d WHERE d.TARGET_ID = t.TARGET_ID) WHERE EXISTS (SELECT 1 FROM SGACC.DML_TEMP_DELTA d WHERE d.TARGET_ID = t.TARGET_ID)",
    );
    assert!(edge(
        &graph,
        "SGACC.DML_TARGET.TARGET_VALUE",
        "SGACC.DML_UPDATE_DELTA.BUMP",
        EdgeKind::DerivesFrom
    ));
    assert!(has_diagnostic(&graph, "SG_TEMPORAL_SELF_LINEAGE"));
    assert!(!has_diagnostic(&graph, "SG_RELATION_UNRESOLVED"));
    assert!(!graph
        .vertices()
        .any(|vertex| vertex.name == "DML_TEMP_DELTA"));
}

#[test]
fn qualified_temp_symbol_does_not_bind_a_different_schema() {
    let graph = scan_oracle_temp_sequence(
        "CREATE GLOBAL TEMPORARY TABLE SGACC.DML_TEMP_STAGE ON COMMIT PRESERVE ROWS AS SELECT SOURCE_ID AS STAGED_ID, AMOUNT AS STAGED_VALUE FROM SGACC.DML_SOURCE; INSERT INTO SGACC.DML_TARGET (TARGET_ID, TARGET_VALUE, LAST_WRITER) SELECT STAGED_ID, STAGED_VALUE, 'wrong_schema' FROM OTHER.DML_TEMP_STAGE",
    );
    assert!(has_diagnostic(&graph, "SG_RELATION_UNRESOLVED"));
    assert!(!edge(
        &graph,
        "SGACC.DML_TARGET.TARGET_ID",
        "SGACC.DML_SOURCE.SOURCE_ID",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn sqlserver_select_into_maps_persistent_target_and_temp_lineage() {
    let persistent = scan_routine_with(
        "SELECT o.id AS id, o.customer_id AS name INTO audit FROM orders o",
        "sqlserver",
        "sql",
    );
    assert!(edge(&persistent, "s.mutate", "s.audit", EdgeKind::Writes));
    assert!(!edge(&persistent, "s.mutate", "s.audit", EdgeKind::Reads));
    assert!(edge(
        &persistent,
        "s.mutate",
        "s.audit.id",
        EdgeKind::Writes
    ));
    assert!(edge(
        &persistent,
        "s.audit.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));

    let temporary = scan_routine_with(
        "SELECT o.id AS id, o.customer_id AS name INTO #tmp FROM orders o; INSERT INTO audit (id, name) SELECT id, name FROM #tmp",
        "sqlserver",
        "sql",
    );
    assert!(edge(
        &temporary,
        "s.audit.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &temporary,
        "s.audit.name",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(!temporary.vertices().any(|vertex| vertex.name == "#tmp"));
}

#[test]
fn insert_select_maps_target_writes_reads_and_value_lineage() {
    let graph = scan_routine(
        "INSERT INTO audit (id, name) SELECT o.id, c.name FROM orders o JOIN customers c ON c.id=o.customer_id WHERE o.amount > 0",
    );
    assert!(edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Writes));
    assert!(edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes));
    assert!(!edge(&graph, "s.mutate", "s.audit", EdgeKind::Reads));
    assert!(edge(&graph, "s.mutate", "s.orders.id", EdgeKind::Reads));
    assert!(edge(&graph, "s.mutate", "s.orders.amount", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.mutate",
        "s.customers.name",
        EdgeKind::Reads
    ));
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &graph,
        "s.audit.id",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn update_from_maps_predicate_reads_without_value_lineage() {
    let graph =
        scan_routine("UPDATE orders o SET customer_id = c.id FROM customers c WHERE c.id = o.id");
    assert!(edge(
        &graph,
        "s.mutate",
        "s.orders.customer_id",
        EdgeKind::Writes
    ));
    assert!(edge(&graph, "s.mutate", "s.customers.id", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.orders.customer_id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&graph, "s.mutate", "s.orders.id", EdgeKind::Reads));
}

#[test]
fn update_from_duplicate_target_keeps_target_alias_and_join_sources_distinct() {
    let graph = scan_routine(
        "UPDATE orders o SET customer_id = c.id FROM orders o JOIN customers c ON c.id = o.id",
    );
    assert!(edge(
        &graph,
        "s.orders.customer_id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_diagnostic(&graph, "SG_COLUMN_AMBIGUOUS"));
}

#[test]
fn sqlserver_update_alias_promotes_the_unique_from_relation() {
    let graph = scan_routine_with(
        "UPDATE o SET amount = o.amount + c.id FROM orders o JOIN customers c ON c.id = o.customer_id",
        "sqlserver",
        "sql",
    );
    assert!(edge(&graph, "s.mutate", "s.orders", EdgeKind::Writes));
    assert!(!edge(&graph, "s.mutate", "s.orders", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.mutate",
        "s.orders.amount",
        EdgeKind::Writes
    ));
    assert!(edge(&graph, "s.mutate", "s.orders.amount", EdgeKind::Reads));
    assert!(edge(&graph, "s.mutate", "s.customers.id", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.orders.amount",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"));
}

#[test]
fn oracle_tuple_assignment_maps_known_width_positionally() {
    let graph = scan_routine_with(
        "UPDATE \"audit\" a SET (\"id\", \"name\") = (SELECT o.\"id\", c.\"name\" FROM \"orders\" o JOIN \"customers\" c ON c.\"id\" = o.\"customer_id\" WHERE o.\"id\" = a.\"id\")",
        "oracle",
        "sql",
    );
    assert!(edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Writes));
    assert!(edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes));
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(!has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"));
}

#[test]
fn merge_maps_match_update_and_not_matched_insert_members() {
    let graph = scan_routine(
        "MERGE INTO audit a USING customers c ON a.id = c.id WHEN MATCHED THEN UPDATE SET name = c.name WHEN NOT MATCHED THEN INSERT (id, name) VALUES (c.id, c.name)",
    );
    assert!(edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes));
    assert!(!edge(&graph, "s.mutate", "s.audit", EdgeKind::Reads));
    assert!(edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Reads));
    assert!(edge(&graph, "s.mutate", "s.customers.id", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Writes));
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

/// 앞에 WITH가 붙은 DML이다. sqlparser는 이 문장을 `Query`로 감싸고 DML을 그 몸체에 둔다.
const WITH_INSERT: &str = "WITH big AS (SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.amount > 0) INSERT INTO audit (id, name) SELECT id, name FROM big";

/// 앞에 WITH가 붙은 INSERT도 일반 INSERT처럼 대상 쓰기와 값 계보를 남긴다. 쿼리 파일은 읽기
/// 전용 SELECT 경로로 가서 `writes` 없이 `SG_SET_UNSUPPORTED`만 남았고, routine은 객체 단위
/// 간선만 남긴 채 컬럼 효과 없이 complete였다.
#[test]
fn with_prefixed_insert_maps_writes_and_lineage() {
    for (dialect, kind) in [
        ("postgres", "query"),
        ("postgres", "procedure"),
        ("sqlite", "query"),
        ("sqlserver", "query"),
    ] {
        let graph = scan_routine_kind(WITH_INSERT, dialect, "sql", kind);
        let detail = format!(
            "{dialect} {kind}: {:?} {:?}",
            graph.edges(),
            graph.limitations()
        );
        assert_eq!(
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
            "{detail}"
        );
        for column in ["s.audit.id", "s.audit.name"] {
            assert!(
                edge(&graph, "s.mutate", column, EdgeKind::Writes),
                "{detail}"
            );
        }
        assert!(
            edge(&graph, "s.audit.id", "s.orders.id", EdgeKind::DerivesFrom),
            "{detail}"
        );
        assert!(
            edge(
                &graph,
                "s.audit.name",
                "s.customers.name",
                EdgeKind::DerivesFrom
            ),
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.orders.amount", EdgeKind::Reads),
            "{detail}"
        );
        assert!(
            !edge(&graph, "s.mutate", "s.audit", EdgeKind::Reads),
            "{detail}"
        );
        assert!(!touches(&graph, "s.mutate", "s.big"), "{detail}");
    }
}

/// 앞에 WITH가 붙은 UPDATE의 FROM·SET·WHERE는 그 CTE를 본다. CTE를 모른 채 분석하면 `big`이
/// 없는 관계가 되어 값 계보가 빠진다.
#[test]
fn with_prefixed_update_reads_the_cte_in_from_set_and_where() {
    let body = "WITH big AS (SELECT c.id, c.name FROM customers c) UPDATE audit SET name = big.name FROM big WHERE audit.id = big.id";
    for kind in ["query", "procedure"] {
        let graph = scan_routine_kind(body, "postgres", "sql", kind);
        let detail = format!("{kind}: {:?} {:?}", graph.edges(), graph.limitations());
        assert_eq!(
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes),
            "{detail}"
        );
        assert!(
            edge(
                &graph,
                "s.audit.name",
                "s.customers.name",
                EdgeKind::DerivesFrom
            ),
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Reads),
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.customers.id", EdgeKind::Reads),
            "{detail}"
        );
        assert!(!touches(&graph, "s.mutate", "s.big"), "{detail}");
    }
}

/// 앞에 WITH가 붙은 DELETE 쿼리 파일은 읽기 전용 SELECT가 아니다. 삭제 대상 쓰기와 CTE가
/// 읽는 관계를 남기고, CTE 이름을 관계로 보지 않는다.
#[test]
fn with_prefixed_delete_query_file_writes_its_target() {
    let graph = scan_routine_kind(
        "WITH stale AS (SELECT id FROM orders WHERE amount < 0) DELETE FROM audit WHERE id IN (SELECT id FROM stale)",
        "sqlite",
        "sql",
        "query",
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        edge(&graph, "s.mutate", "s.orders", EdgeKind::Reads),
        "{detail}"
    );
    assert!(!has_diagnostic(&graph, "SG_SET_UNSUPPORTED"), "{detail}");
    assert!(!touches(&graph, "s.mutate", "s.stale"), "{detail}");
}

/// SQL Server는 CTE를 통한 UPDATE·DELETE를 허용한다(쓰기는 CTE의 원본 테이블로 간다).
/// 대상 테이블을 추측하지 않고 부분 분석으로 남긴다 — CTE 이름으로 가는 쓰기 간선이나 "없는
/// 관계" 한계, 같은 이름의 카탈로그 테이블로 가는 거짓 쓰기, 원본 컬럼 쓰기를 만들지 않는다.
/// 중복 행 삭제 관용구(`WITH dupes AS (… ROW_NUMBER() …) DELETE FROM dupes …`)와 절차형
/// 블록 안의 문장(`IF 1 = 1 WITH …`, MySQL `IF … THEN WITH …`)도 같다 — 부모 문장이 안쪽 쓰기
/// 대상을 모으던 경로에서 거짓 `writes`가 남았다.
#[test]
fn sqlserver_dml_through_a_cte_is_partial_without_guessed_targets() {
    for (dialect, kind, body, target) in [
        (
            "sqlserver",
            "query",
            "WITH named AS (SELECT id, name FROM customers) UPDATE named SET name = 'x'",
            "named",
        ),
        // `UPDATE 별칭 … FROM 관계 별칭`은 별칭이 가리키는 FROM 관계가 대상이다.
        (
            "sqlserver",
            "query",
            "WITH named AS (SELECT id, name FROM customers) UPDATE n SET name = 'x' FROM named n",
            "named",
        ),
        // 같은 이름의 카탈로그 테이블이 있어도 SQL Server는 CTE를 통해 customers에 쓴다.
        (
            "sqlserver",
            "query",
            "WITH audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'",
            "audit",
        ),
        (
            "sqlserver",
            "query",
            "WITH dupes AS (SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) AS rn FROM customers) DELETE FROM dupes WHERE rn > 1",
            "dupes",
        ),
        (
            "sqlserver",
            "procedure",
            "IF 1 = 1 WITH audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'",
            "audit",
        ),
        (
            "mysql",
            "procedure",
            "BEGIN IF 1 = 1 THEN WITH audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'; END IF; END",
            "audit",
        ),
    ] {
        let graph = scan_routine_kind(body, dialect, "sql", kind);
        let detail = format!("{body}: {:?} {:?}", graph.edges(), graph.limitations());
        let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
        assert!(
            analysis.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "SG_DML_TARGET_SHAPE"
                    && diagnostic.message.contains(&format!("'{target}' names a WITH query"))
            }),
            "{detail}"
        );
        assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.from.as_str() != "s.mutate" || edge.kind != EdgeKind::Writes),
            "{detail}"
        );
        let target = format!("s.{target}");
        assert!(!touches(&graph, "s.mutate", &target), "{detail}");
        assert!(
            edge(&graph, "s.mutate", "s.customers", EdgeKind::Reads),
            "{detail}"
        );
        assert!(
            !graph.limitations().iter().any(|note| note.contains(&target)),
            "{detail}"
        );
    }
}

/// CTE 이름 대상의 뜻을 확인하지 않은 방언(MariaDB·H2·`external` 등)은 객체 단위 쓰기를 이전처럼
/// 그 이름에 남기되 `complete`를 주장하지 않는다 — 이름 붙은 진단과 부분 분석, 추측한 컬럼 효과
/// 없음. DELETE는 컬럼 분석기가 대상을 보지 않아, 객체 단위 적용이 진단을 남기지 않으면 진단 없이
/// complete가 됐다.
#[test]
fn unverified_dialects_keep_the_object_write_of_a_cte_named_target() {
    const UNVERIFIED: &str = "DML target 'audit' names a WITH query; this dialect's rule for such targets is unverified; the write edge is kept, but target columns are not resolved";
    // 철자가 다르면 같은 이름이라고 단정하지 않는다.
    const UNVERIFIED_CASE: &str = "DML target 'audit' may name the WITH query 'Audit'; this dialect's rule for such targets is unverified; the write edge is kept, but target columns are not resolved";
    for dialect in ["mariadb", "h2", "external"] {
        for (kind, body) in [
            (
                "procedure",
                "WITH audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'",
            ),
            (
                "procedure",
                "WITH audit AS (SELECT id, name FROM customers WHERE id < 0) DELETE FROM audit",
            ),
            (
                "query",
                "WITH audit AS (SELECT id, name FROM customers WHERE id < 0) DELETE FROM audit",
            ),
            // 대소문자만 다른 이름도 같다 — 방언 규칙을 모르면 대소문자 규칙도 판정하지 않는다.
            (
                "procedure",
                "WITH Audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'",
            ),
        ] {
            let graph = scan_routine_kind(body, dialect, "sql", kind);
            let detail = format!(
                "{dialect} {kind} {body}: {:?} {:?}",
                graph.edges(),
                graph.limitations()
            );
            let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
            assert!(
                edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
                "{detail}"
            );
            assert!(
                !edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes),
                "{detail}"
            );
            assert!(
                !edge(&graph, "s.mutate", "s.customers.name", EdgeKind::Writes),
                "{detail}"
            );
            let expected = if body.starts_with("WITH Audit") {
                UNVERIFIED_CASE
            } else {
                UNVERIFIED
            };
            assert!(
                analysis
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message == expected),
                "{detail}"
            );
            assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        }
    }
}

/// MySQL은 `lower_case_table_names`가 CTE 이름 비교를 정한다(0이면 그대로, 1·2면 소문자로 바꿔
/// 비교). 값이 없으면 대소문자만 다른 대상을 판정하지 않는다 — 쓰기는 남기고 부분 분석이다.
#[test]
fn mysql_cte_target_names_follow_lower_case_table_names() {
    let context = |lctn: Option<u8>| CollectionContext {
        source_id: "fixture".into(),
        database: None,
        schema_filter: None,
        catalog_complete: true,
        lower_case_table_names: lctn,
        collation: None,
    };
    let body = "WITH Audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'";
    let scan =
        |lctn| scan_routine_in_context(body, "mysql", "sql", "procedure", Some(context(lctn)));
    let sensitive = scan(Some(0));
    assert!(edge(
        &sensitive,
        "s.mutate",
        "s.audit.name",
        EdgeKind::Writes
    ));
    assert!(!has_diagnostic(&sensitive, "SG_DML_TARGET_SHAPE"));
    for lctn in [Some(1), Some(2)] {
        let folded = scan(lctn);
        let detail = format!("{lctn:?}: {:?}", folded.edges());
        assert!(!touches(&folded, "s.mutate", "s.audit"), "{detail}");
        assert!(
            folded.analysis()[&VertexId::from_raw("s.mutate")]
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("MySQL rejects a WITH query")),
            "{detail}"
        );
    }
    let unknown = scan(None);
    assert!(edge(&unknown, "s.mutate", "s.audit", EdgeKind::Writes));
    assert!(!edge(
        &unknown,
        "s.mutate",
        "s.audit.name",
        EdgeKind::Writes
    ));
    assert!(has_diagnostic(&unknown, "SG_DML_TARGET_SHAPE"));
}

/// 앞에 WITH가 붙은 DELETE는 절차형 블록 판정에서 WITH 없는 DELETE와 같다 — 컬럼 DML이 아니므로
/// `SG_DML_CONTROL_FLOW`를 만들지 않고, CTE 몸체의 컬럼 읽기만 더한다.
#[test]
fn with_prefixed_delete_in_control_flow_is_not_column_dml() {
    for (dialect, body) in [
        (
            "postgres",
            "BEGIN IF true THEN WITH c AS (SELECT id FROM orders) DELETE FROM audit WHERE id IN (SELECT id FROM c); END IF; END",
        ),
        (
            "mysql",
            "BEGIN CASE WHEN 1 = 1 THEN WITH c AS (SELECT id FROM orders) DELETE FROM audit WHERE id IN (SELECT id FROM c); END CASE; END",
        ),
    ] {
        let graph = scan_routine_kind(body, dialect, "sql", "procedure");
        let detail = format!("{dialect}: {:?} {:?}", graph.edges(), graph.limitations());
        assert!(!has_diagnostic(&graph, "SG_DML_CONTROL_FLOW"), "{detail}");
        assert_eq!(
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
            "{detail}"
        );
        assert!(
            edge(&graph, "s.mutate", "s.orders.id", EdgeKind::Reads),
            "{detail}"
        );
    }
}

/// CTE 이름 대상은 대상마다 따로 센다. 하나로 합치면 두 번째 대상이 사라진다.
#[test]
fn cte_target_diagnostics_name_each_target() {
    let graph = scan_routine_kind(
        "WITH named AS (SELECT id, name FROM customers) UPDATE named SET name = 'x'; WITH n2 AS (SELECT id FROM orders) DELETE FROM n2",
        "sqlserver",
        "sql",
        "procedure",
    );
    let messages: Vec<&str> = graph.analysis()[&VertexId::from_raw("s.mutate")]
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "SG_DML_TARGET_SHAPE")
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    for target in ["named", "n2"] {
        assert!(
            messages
                .iter()
                .any(|message| message.contains(&format!("'{target}' names a WITH query"))),
            "{messages:?}"
        );
    }
}

/// CTE 이름 판정 테스트의 수집 context다.
fn naming_context(collation: Option<&str>, lctn: Option<u8>) -> CollectionContext {
    CollectionContext {
        source_id: "fixture".into(),
        database: None,
        schema_filter: None,
        catalog_complete: true,
        lower_case_table_names: lctn,
        collation: collation.map(str::to_owned),
    }
}

/// SQL Server 문서는 어느 collation이 CTE 이름을 비교하는지 말하지 않는다 — 데이터베이스 안 객체
/// 식별자는 데이터베이스 collation, 변수·GOTO 레이블·임시 테이블은 인스턴스 collation, 포함된
/// 데이터베이스는 카탈로그 collation이라고만 한다. 그래서 대상과 CTE 이름이 대소문자만 다르면
/// 문서의 데이터베이스 collation이 무엇이든(`_CI`·`_CS`·`_BIN2`·터키어·없음) 판정하지 않는다 —
/// 쓰기는 남기고 이름 붙은 진단과 부분 분석, 컬럼 효과 없음. 이전에는 `_CI`에서 쓰기를 지우고
/// `_CS`·`_BIN2`에서 실제 테이블로 단정해 `complete`를 냈다. 철자가 정확히 같으면 어느 collation이든
/// CTE다("Any reference to expression_name in the query uses the common table expression").
#[test]
fn sqlserver_case_only_cte_targets_are_unproven_under_every_collation() {
    let scan = |body: &str, collation: Option<&str>| {
        scan_routine_in_context(
            body,
            "sqlserver",
            "sql",
            "procedure",
            Some(naming_context(collation, None)),
        )
    };
    let update = "WITH Audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'";
    let delete = "WITH Audit AS (SELECT id, name FROM customers) DELETE FROM audit WHERE id = 1";
    let exact = "WITH audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'";
    for collation in [
        None,
        Some("SQL_Latin1_General_CP1_CI_AS"),
        Some("Latin1_General_CS_AS"),
        Some("Latin1_General_BIN2"),
        Some("Turkish_CI_AS"),
        Some("Latin1_General_100_CI_AI_SC_UTF8"),
    ] {
        for body in [update, delete] {
            let graph = scan(body, collation);
            let detail = format!(
                "{body} {collation:?}: {:?} {:?}",
                graph.edges(),
                graph.limitations()
            );
            let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
            assert!(
                edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
                "{detail}"
            );
            assert!(
                !edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes),
                "{detail}"
            );
            assert!(
                analysis.diagnostics.iter().any(|diagnostic| diagnostic.message
                    == "DML target 'audit' may name the WITH query 'Audit'; the names differ only in letter case and the case rule for WITH names is not proven (SQL Server does not document which collation compares them); the write edge is kept, but target columns are not resolved"),
                "{detail}"
            );
            assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        }
        let graph = scan(exact, collation);
        let detail = format!("exact {collation:?}: {:?}", graph.edges());
        assert!(!touches(&graph, "s.mutate", "s.audit"), "{detail}");
        assert!(
            graph.analysis()[&VertexId::from_raw("s.mutate")]
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("SQL Server writes through it")),
            "{detail}"
        );
    }
}

/// 대상과 CTE 이름의 바이트가 다르고 ASCII 밖 글자가 있으면 바이트 비교 규칙(MySQL
/// `lower_case_table_names`=0)에서만 다른 이름으로 증명된다. 접는 규칙(SQL Server `_CI`·`Greek_CI`,
/// 폭을 무시하는 `_CS_AS`, MySQL 1·2)이나 모르는 규칙(collation 없음, MariaDB·`external`)에서는
/// 판정하지 않는다 — 쓰기는 남기고 이름 붙은 진단과 부분 분석. 이전에는 `str::to_lowercase`로
/// 비교해 그리스어 어말 시그마(`ΑΣ`→`ας`)·전각(`ａｕｄｉｔ`)·악센트(`äudit`) 차이를 "다른 이름"으로
/// 단정했고, 실제 테이블 쓰기와 추측한 컬럼 쓰기, `complete`를 냈다. MySQL 1·2에서 ASCII 밖 글자가
/// 섞인 이름(`étAPE`·`étape`)은 대소문자만 다르다고 단정해 쓰기를 지웠다.
#[test]
fn non_ascii_cte_target_names_are_unproven_under_folding_rules() {
    let cases = [
        // (CTE 이름, 대상)
        ("ασ", "ΑΣ"),
        ("ａｕｄｉｔ", "audit"),
        ("äudit", "audit"),
        // ASCII 글자의 대소문자만 다르더라도 ASCII 밖 글자가 섞이면 ASCII 접기로 증명하지 않는다.
        ("étAPE", "étape"),
    ];
    let scan = |dialect: &str, context: Option<CollectionContext>, cte: &str, target: &str| {
        scan_routine_with_objects(
            &format!(
                "WITH {cte} AS (SELECT id, name FROM customers) UPDATE {target} SET name = 'x'"
            ),
            dialect,
            "sql",
            "procedure",
            context,
            vec![
                object("ΑΣ", &["id", "name"]),
                object("étape", &["id", "name"]),
            ],
        )
    };
    for (cte, target) in cases {
        for (dialect, context) in [
            (
                "sqlserver",
                Some(naming_context(Some("SQL_Latin1_General_CP1_CI_AS"), None)),
            ),
            ("sqlserver", Some(naming_context(Some("Greek_CI_AS"), None))),
            (
                "sqlserver",
                Some(naming_context(Some("Latin1_General_CS_AS"), None)),
            ),
            ("sqlserver", None),
            ("mysql", Some(naming_context(None, Some(1)))),
            ("mysql", Some(naming_context(None, Some(2)))),
            ("mysql", None),
            ("mariadb", None),
            ("external", None),
        ] {
            let graph = scan(dialect, context.clone(), cte, target);
            let detail = format!(
                "{dialect} {context:?} {cte}/{target}: {:?} {:?}",
                graph.edges(),
                graph.limitations()
            );
            let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
            assert!(
                edge(&graph, "s.mutate", &format!("s.{target}"), EdgeKind::Writes),
                "{detail}"
            );
            assert!(
                !edge(
                    &graph,
                    "s.mutate",
                    &format!("s.{target}.name"),
                    EdgeKind::Writes
                ),
                "{detail}"
            );
            assert!(
                analysis.diagnostics.iter().any(|diagnostic| {
                    diagnostic.code == "SG_DML_TARGET_SHAPE"
                        && diagnostic
                            .message
                            .contains(&format!("'{target}' may name the WITH query"))
                }),
                "{detail}"
            );
            assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        }
        // 바이트 비교(MySQL 0)와 대상을 실제 테이블로 여는 SQLite에서는 평범한 UPDATE다.
        for (dialect, context) in [
            ("mysql", Some(naming_context(None, Some(0)))),
            ("sqlite", None),
        ] {
            let graph = scan(dialect, context, cte, target);
            let detail = format!("{dialect} {cte}/{target}: {:?}", graph.edges());
            assert!(
                edge(
                    &graph,
                    "s.mutate",
                    &format!("s.{target}.name"),
                    EdgeKind::Writes
                ),
                "{detail}"
            );
            assert!(!has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"), "{detail}");
            assert_eq!(
                graph.analysis()[&VertexId::from_raw("s.mutate")].state,
                AnalysisState::Complete,
                "{detail}"
            );
        }
    }
}

/// 대소문자를 무시하는 문서에서 철자만 다른 같은 CTE 이름 대상(`dupes`·`DUPES`)은 한 번만 센다.
/// 그리스어도 같다 — `str::to_lowercase`는 `ΑΣ`를 어말 시그마 `ας`로 바꿔, 그 규칙에서 같은 이름인
/// `ΑΣ`·`ασ`를 두 대상으로 셌다. 철자 그대로 비교하는 문서(MySQL 0)에서는 접지 않는다.
#[test]
fn cte_target_diagnostics_fold_names_under_a_case_insensitive_rule() {
    let messages = |body: &str, lctn: u8| -> Vec<String> {
        scan_routine_in_context(
            body,
            "mysql",
            "sql",
            "procedure",
            Some(naming_context(None, Some(lctn))),
        )
        .analysis()[&VertexId::from_raw("s.mutate")]
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "SG_DML_TARGET_SHAPE")
            .map(|diagnostic| diagnostic.message.clone())
            .collect()
    };
    let dupes = "WITH Dupes AS (SELECT id FROM orders) DELETE FROM dupes; WITH DUPES AS (SELECT id FROM orders) DELETE FROM DUPES";
    let folded = messages(dupes, 1);
    assert_eq!(folded.len(), 1, "{folded:?}");
    assert!(folded[0].contains("'dupes'"), "{folded:?}");

    let sigma = "WITH ΑΣ AS (SELECT id FROM orders) DELETE FROM ΑΣ; WITH ασ AS (SELECT id FROM orders) DELETE FROM ασ";
    let folded = messages(sigma, 1);
    assert_eq!(folded.len(), 1, "{folded:?}");
    assert!(folded[0].contains("'ασ'"), "{folded:?}");
    let exact = messages(sigma, 0);
    assert_eq!(exact.len(), 2, "{exact:?}");
    for target in ["'ΑΣ'", "'ασ'"] {
        assert!(
            exact.iter().any(|message| message.contains(target)),
            "{exact:?}"
        );
    }
}

/// 다중 테이블 DELETE의 대상 목록은 FROM 절 관계를 가리킨다(SQL Server *table_alias*: "The alias
/// specified in the FROM table_source clause representing the table or view from which the rows are
/// to be deleted", mysql-server 8.0은 그 목록을 CTE로 찾지 않는다). 별칭이 CTE 이름과 같아도
/// 별칭이 가리키는 테이블에 쓴다 — 이전에는 `DELETE c FROM audit c …`의 `c`를 CTE로 판정해
/// `audit` 쓰기를 지웠다(WITH가 없으면 `s.c`가 없는 관계라는 한계가 남았다). `UPDATE c … FROM
/// audit c` 승격과 같은 규칙이다. 별칭이 가리키는 FROM 관계가 CTE면 CTE를 통한 DELETE다.
#[test]
fn delete_through_a_from_alias_writes_the_aliased_table() {
    let with_alias =
        "WITH c AS (SELECT id, name FROM customers) DELETE c FROM audit c JOIN orders o ON c.id = o.id";
    let joined_alias =
        "WITH c AS (SELECT id, name FROM customers) DELETE c FROM orders o JOIN audit c ON c.id = o.id";
    let plain = "DELETE c FROM audit c JOIN orders o ON c.id = o.id";
    for (dialect, kind, context) in [
        ("sqlserver", "procedure", None),
        ("sqlserver", "query", None),
        ("mysql", "procedure", Some(naming_context(None, Some(0)))),
        ("mysql", "query", Some(naming_context(None, Some(1)))),
        ("mysql", "procedure", None),
    ] {
        for body in [with_alias, joined_alias, plain] {
            let graph = scan_routine_in_context(body, dialect, "sql", kind, context.clone());
            let detail = format!(
                "{dialect} {kind} {context:?} {body}: {:?} {:?}",
                graph.edges(),
                graph.limitations()
            );
            let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
            assert!(
                edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
                "{detail}"
            );
            assert!(!has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"), "{detail}");
            assert!(!touches(&graph, "s.mutate", "s.c"), "{detail}");
            assert!(
                !graph.limitations().iter().any(|note| note.contains("s.c,")),
                "{detail}"
            );
            assert_eq!(analysis.state, AnalysisState::Complete, "{detail}");
        }
    }
    let through_cte = scan_routine_kind(
        "WITH audit AS (SELECT id, name FROM customers) DELETE x FROM audit x JOIN orders o ON x.id = o.id",
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?}", through_cte.edges());
    assert!(!touches(&through_cte, "s.mutate", "s.audit"), "{detail}");
    assert!(
        through_cte.analysis()[&VertexId::from_raw("s.mutate")]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.starts_with(
                "DML target 'audit' names a WITH query; SQL Server writes through it"
            )),
        "{detail}"
    );
}

/// 수집한 데이터베이스까지 한정한 DELETE·TRUNCATE 대상(`app.s.audit`)은 쓰기다. 세 부분 쓰기 대상은
/// `qualified_write_targets`가 따로 모으는데 DELETE·TRUNCATE가 빠져 있어, 대상이 FROM 관계로 방문되어
/// `reads s.audit`(거짓)이 되고 쓰기 간선은 없었다. 별칭이 세 부분 관계를 가리키는 다중 테이블
/// DELETE·UPDATE(`DELETE c FROM app.s.audit c …`)는 별칭 철자로 되돌아가 없는 관계 `s.c` 한계와 부분
/// 분석을 냈다. SQL Server 문서의 DELETE `<object>`와 TRUNCATE TABLE 문법은 `database_name.schema_name.`
/// 한정을 허용한다.
#[test]
fn database_qualified_delete_and_truncate_targets_are_writes() {
    let context = CollectionContext {
        database: Some("app".into()),
        ..naming_context(None, None)
    };
    for body in [
        "DELETE FROM app.s.audit WHERE id = 1",
        "TRUNCATE TABLE app.s.audit",
        "DELETE c FROM app.s.audit c JOIN s.orders o ON c.id = o.id",
        "UPDATE c SET name = 'x' FROM app.s.audit c JOIN s.orders o ON c.id = o.id",
        "WITH x AS (SELECT id FROM s.orders) DELETE FROM app.s.audit WHERE id IN (SELECT id FROM x)",
        // 별칭 `c`·`C`는 대소문자를 구분하는 collation에서만 함께 쓸 수 있고, 그때 `c`는 audit다.
        // 이전에는 세 부분 관계를 후보에서 빼 `C`(orders)를 대상으로 골랐다.
        "DELETE c FROM app.s.audit c JOIN s.orders C ON c.id = C.id",
    ] {
        let graph = scan_routine_in_context(
            body,
            "sqlserver",
            "sql",
            "procedure",
            Some(context.clone()),
        );
        let detail = format!("{body}: {:?} {:?}", graph.edges(), graph.limitations());
        assert!(
            edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
            "{detail}"
        );
        assert!(
            !edge(&graph, "s.mutate", "s.audit", EdgeKind::Reads),
            "{detail}"
        );
        assert!(
            !edge(&graph, "s.mutate", "s.orders", EdgeKind::Writes),
            "{detail}"
        );
        assert!(!touches(&graph, "s.mutate", "s.c"), "{detail}");
        assert!(graph.limitations().is_empty(), "{detail}");
        assert_eq!(
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
    }
    // 다른 데이터베이스의 대상은 이전처럼 간선 없이 SG_CROSS_DATABASE다.
    let other = scan_routine_in_context(
        "DELETE FROM other.s.audit WHERE id = 1",
        "sqlserver",
        "sql",
        "procedure",
        Some(context),
    );
    let detail = format!("{:?} {:?}", other.edges(), other.limitations());
    assert!(!touches(&other, "s.mutate", "s.audit"), "{detail}");
    assert!(has_diagnostic(&other, "SG_CROSS_DATABASE"), "{detail}");
}

/// MySQL·MariaDB의 `DELETE FROM c USING audit c …`에서 FROM 목록은 USING 관계를 가리키는 참조다
/// (mysql-server 8.0 `sql_yacc.yy`: `FROM table_alias_ref_list USING table_reference_list`). 이전에는
/// FROM의 `c`를 삭제 대상 테이블로 읽어 `s.c`가 없다는 거짓 한계와 `reads s.audit`만 남겼다.
/// PostgreSQL의 FROM은 삭제할 테이블 자체라 같은 모양을 풀지 않는다 — `DELETE FROM c USING audit "C"`는
/// PostgreSQL에서 테이블 `c`를 지운다(PostgreSQL 16에서 확인).
#[test]
fn mysql_delete_using_resolves_the_from_list_through_using_aliases() {
    for dialect in ["mysql", "mariadb"] {
        for (body, writes, reads) in [
            (
                "DELETE FROM c USING audit c JOIN orders o ON c.id = o.id",
                &["s.audit"][..],
                &["s.orders"][..],
            ),
            (
                "DELETE FROM c, o USING audit AS c JOIN orders AS o ON c.id = o.id",
                &["s.audit", "s.orders"][..],
                &[][..],
            ),
            (
                "DELETE FROM audit USING audit JOIN orders o ON audit.id = o.id",
                &["s.audit"][..],
                &["s.orders"][..],
            ),
            (
                "WITH c AS (SELECT id FROM customers) DELETE FROM c USING audit c JOIN orders o ON c.id = o.id",
                &["s.audit"][..],
                &["s.orders", "s.customers"][..],
            ),
        ] {
            let graph = scan_routine_in_context(
                body,
                dialect,
                "sql",
                "procedure",
                Some(naming_context(None, Some(0))),
            );
            let detail = format!(
                "{dialect} {body}: {:?} {:?}",
                graph.edges(),
                graph.limitations()
            );
            for target in writes {
                assert!(
                    edge(&graph, "s.mutate", target, EdgeKind::Writes),
                    "{detail}"
                );
            }
            for source in reads {
                assert!(edge(&graph, "s.mutate", source, EdgeKind::Reads), "{detail}");
            }
            assert!(!touches(&graph, "s.mutate", "s.c"), "{detail}");
            assert!(graph.limitations().is_empty(), "{detail}");
            assert_eq!(
                graph.analysis()[&VertexId::from_raw("s.mutate")].state,
                AnalysisState::Complete,
                "{detail}"
            );
        }
    }
    // USING 별칭이 그 문장의 WITH 이름인 관계를 가리키면 MySQL은 그 WITH 질의를 대상으로 본다.
    let through_cte = scan_routine_in_context(
        "WITH audit AS (SELECT id FROM customers) DELETE FROM x USING audit x JOIN orders o ON x.id = o.id",
        "mysql",
        "sql",
        "procedure",
        Some(naming_context(None, Some(0))),
    );
    let detail = format!("{:?}", through_cte.edges());
    assert!(!touches(&through_cte, "s.mutate", "s.audit"), "{detail}");
    assert!(
        has_diagnostic(&through_cte, "SG_DML_TARGET_SHAPE"),
        "{detail}"
    );
    // PostgreSQL은 FROM의 테이블을 지운다.
    let postgres = scan_routine_with_objects(
        "DELETE FROM c USING audit \"C\" WHERE \"C\".id = c.id",
        "postgres",
        "sql",
        "procedure",
        None,
        vec![object("c", &["id"])],
    );
    let detail = format!("{:?} {:?}", postgres.edges(), postgres.limitations());
    assert!(
        edge(&postgres, "s.mutate", "s.c", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        !edge(&postgres, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
}

/// 세미콜론을 생략한 T-SQL의 `BEGIN TRY … END TRY BEGIN CATCH … END CATCH`(문법: MicrosoftDocs
/// `try-catch-transact-sql.md`, 세미콜론은 대부분 문장에서 선택: `transact-sql-syntax-conventions…`)는
/// 세미콜론을 붙인 같은 몸체와 같은 간선을 남긴다. sqlparser는 `;` 없는 문장 뒤의 `END`에서 멈추고
/// 성공을 돌려줘, `END TRY` 뒤의 CATCH 문장이 소리 없이 빠진 채 `complete`였다.
#[test]
fn tsql_try_catch_without_semicolons_keeps_the_statements_after_end() {
    let without = "BEGIN TRY\n  UPDATE audit SET name = 'x' WHERE id = 1\nEND TRY\nBEGIN CATCH\n  INSERT INTO orders (id, amount) SELECT id, id FROM customers\nEND CATCH";
    let with = "BEGIN TRY\n  UPDATE audit SET name = 'x' WHERE id = 1;\nEND TRY\nBEGIN CATCH\n  INSERT INTO orders (id, amount) SELECT id, id FROM customers;\nEND CATCH";
    let facts = |body: &str| {
        let graph = scan_routine_kind(body, "sqlserver", "sql", "procedure");
        let mut edges: Vec<String> = graph
            .edges()
            .iter()
            .filter(|edge| edge.from.as_str() == "s.mutate")
            .map(|edge| format!("{:?} {}", edge.kind, edge.to))
            .collect();
        edges.sort();
        let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
        (edges, analysis.state, graph.limitations().to_vec())
    };
    let (edges, state, limitations) = facts(without);
    let detail = format!("{edges:?} {limitations:?}");
    for expected in [
        "Writes s.audit",
        "Writes s.audit.name",
        "Writes s.orders",
        "Writes s.orders.amount",
        "Reads s.customers",
    ] {
        assert!(edges.iter().any(|edge| edge == expected), "{detail}");
    }
    assert_eq!(state, AnalysisState::Complete, "{detail}");
    assert_eq!((edges, state, limitations), facts(with));
}

/// 최상위 `END` 뒤를 다시 읽지 못하면 그 앞 문장의 간선은 남기되 `complete`를 주장하지 않는다.
/// 절차형 추출로 가는 몸체는 남은 조각을 미추출로 세고, 통째로 읽은 몸체는 이름 붙은
/// `SG_BODY_PARTIAL` 진단을 단다. 이전에는 둘 다 뒤 문장을 소리 없이 버리고 `complete`였다.
#[test]
fn statements_after_an_unparsed_top_level_end_are_not_claimed_complete() {
    let whole = scan_routine_kind(
        "UPDATE audit SET name = 'x' END TRY UPDATE orders SET amount = 0",
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?} {:?}", whole.edges(), whole.analysis());
    let analysis = &whole.analysis()[&VertexId::from_raw("s.mutate")];
    assert!(
        edge(&whole, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
    assert!(!touches(&whole, "s.mutate", "s.orders"), "{detail}");
    assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
    assert!(
        analysis.diagnostics.iter().any(|diagnostic| diagnostic.code == "SG_BODY_PARTIAL"
            && diagnostic.message.starts_with(
                "body text from a top-level END that follows a statement without a semicolon was not parsed"
            )),
        "{detail}"
    );
    // DECLARE가 있어 절차형 추출로 가는 몸체: `END` 뒤 조각은 같은 추출기로 다시 꺼낸다. 추출기는
    // `END` 뒤 한 단어를 블록 꼬리로 벗기고, 남은 `orders`처럼 문장 머리가 아닌 조각은 미추출로 센다.
    let extracted = scan_routine_kind(
        "DECLARE @n int; UPDATE audit SET name = 'x' END frobnicate orders",
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?} {:?}", extracted.edges(), extracted.limitations());
    assert!(
        edge(&extracted, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
    assert_eq!(
        extracted.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Partial,
        "{detail}"
    );
    assert!(
        extracted
            .limitations()
            .iter()
            .any(|note| note.contains("body statement(s) not extracted")),
        "{detail}"
    );
}

/// 세미콜론 없는 조각에서 최상위 `END` 뒤를 다시 꺼내는 일은 조각마다 `MAX_REEXTRACTIONS`번까지다.
/// 다시 꺼낼 때마다 남은 텍스트를 다시 토큰화하므로, 상한이 없으면 `TRY`·`CATCH`가 많은 몸체에서
/// 일이 블록 수 × 길이로 커진다. 상한 안(여덟 쌍)은 모두 쓰기 간선이 되고, 넘은 나머지는
/// 미추출로 세어 부분 분석이다.
#[test]
fn reextraction_after_end_is_bounded_per_piece() {
    let scan_pairs = |pairs: usize| {
        let tables: Vec<ObjectDoc> = (0..2 * pairs)
            .map(|index| object(&format!("t{index}"), &["id"]))
            .collect();
        let body: String = (0..pairs)
            .map(|pair| {
                format!(
                    "BEGIN TRY UPDATE t{} SET id = 1 END TRY BEGIN CATCH UPDATE t{} SET id = 2 END CATCH ",
                    2 * pair,
                    2 * pair + 1
                )
            })
            .collect();
        let graph = scan_routine_with_objects(&body, "sqlserver", "sql", "procedure", None, tables);
        let written = (0..2 * pairs)
            .filter(|index| edge(&graph, "s.mutate", &format!("s.t{index}"), EdgeKind::Writes))
            .count();
        let state = graph.analysis()[&VertexId::from_raw("s.mutate")].state;
        (written, state, graph.limitations().to_vec())
    };
    let within = super::MAX_REEXTRACTIONS / 2;
    let (written, state, limitations) = scan_pairs(within);
    assert_eq!(written, 2 * within, "{limitations:?}");
    assert_eq!(state, AnalysisState::Complete, "{limitations:?}");
    let (written, state, limitations) = scan_pairs(within + 1);
    assert_eq!(written, 2 * within + 1, "{limitations:?}");
    assert_eq!(state, AnalysisState::Partial, "{limitations:?}");
    assert!(
        limitations
            .iter()
            .any(|note| note.contains("1 body statement(s) not extracted")),
        "{limitations:?}"
    );
}

/// 상한에 걸리면 미추출 수(조각 하나)만으로는 손실의 크기를 알 수 없어, 남은 바이트 수를 진단에 싣는다.
#[test]
fn reextraction_cap_reports_the_size_left_unanalyzed() {
    let pairs = super::MAX_REEXTRACTIONS;
    let body: String = (0..pairs)
        .map(|_| "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH UPDATE orders SET amount = 0 END CATCH ")
        .collect();
    let graph = scan_routine_kind(&body, "sqlserver", "sql", "procedure");
    let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
    assert_eq!(analysis.state, AnalysisState::Partial);
    assert!(
        analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "SG_BODY_PARTIAL"
                && diagnostic.message.starts_with(&format!(
                    "statement extraction stopped after {} re-extractions",
                    super::MAX_REEXTRACTIONS
                ))
                && diagnostic
                    .message
                    .contains("bytes, which may hold many statements")
        }),
        "{:?}",
        analysis.diagnostics
    );
}

/// 세미콜론 없는 T-SQL 조각에서 추출기가 무시하는 문장(`PRINT`·`RAISERROR`·`WAITFOR`·`GOTO`·`THROW`)과
/// `EXEC proc` 인자는 다음 문장 머리 앞에서 끝난다. 이전에는 그 문장이 조각 끝까지를 삼켜 뒤 문장이
/// 소리 없이 빠진 채 `complete`였다. 세미콜론을 붙인 같은 몸체와 같은 결과여야 한다.
#[test]
fn ignored_tsql_statements_end_at_the_next_statement_head() {
    let facts = |body: &str| {
        let graph = scan_routine_kind(body, "sqlserver", "sql", "procedure");
        let mut edges: Vec<String> = graph
            .edges()
            .iter()
            .filter(|edge| edge.from.as_str() == "s.mutate")
            .map(|edge| format!("{:?} {}", edge.kind, edge.to))
            .collect();
        edges.sort();
        let state = graph.analysis()[&VertexId::from_raw("s.mutate")].state;
        (edges, state, graph.limitations().to_vec())
    };
    for (without, with, written) in [
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH PRINT ERROR_MESSAGE() INSERT INTO orders (id) VALUES (1) END CATCH",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH PRINT ERROR_MESSAGE(); INSERT INTO orders (id) VALUES (1); END CATCH",
            "Writes s.orders",
        ),
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH RAISERROR('x', 10, 1) WITH NOWAIT INSERT INTO orders (id) VALUES (1) END CATCH",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH RAISERROR('x', 10, 1) WITH NOWAIT; INSERT INTO orders (id) VALUES (1); END CATCH",
            "Writes s.orders",
        ),
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH WAITFOR DELAY '00:00:01' INSERT INTO orders (id) VALUES (1) END CATCH",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH WAITFOR DELAY '00:00:01'; INSERT INTO orders (id) VALUES (1); END CATCH",
            "Writes s.orders",
        ),
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH GOTO retry INSERT INTO orders (id) VALUES (1) END CATCH",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH GOTO retry; INSERT INTO orders (id) VALUES (1); END CATCH",
            "Writes s.orders",
        ),
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH THROW 50000, 'x', 1 END CATCH UPDATE orders SET amount = 0",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH THROW 50000, 'x', 1; END CATCH; UPDATE orders SET amount = 0",
            "Writes s.orders",
        ),
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH EXEC log_error @code = 1 INSERT INTO orders (id) VALUES (1) END CATCH",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH EXEC log_error @code = 1; INSERT INTO orders (id) VALUES (1); END CATCH",
            "Writes s.orders",
        ),
        // 변수 이름 안의 `update`는 문장 머리가 아니다.
        (
            "DECLARE @update int; PRINT @update INSERT INTO orders (id) VALUES (1)",
            "DECLARE @update int; PRINT @update; INSERT INTO orders (id) VALUES (1)",
            "Writes s.orders",
        ),
        // `CASE … END`의 END는 다음 문장이 아니다.
        (
            "DECLARE @n int; PRINT CASE WHEN @n = 1 THEN 'a' ELSE 'b' END UPDATE orders SET amount = 0",
            "DECLARE @n int; PRINT CASE WHEN @n = 1 THEN 'a' ELSE 'b' END; UPDATE orders SET amount = 0",
            "Writes s.orders",
        ),
    ] {
        let (edges, state, limitations) = facts(without);
        assert!(
            edges.iter().any(|edge| edge == written),
            "{without}: {edges:?} {limitations:?}"
        );
        assert_eq!((edges, state, limitations), facts(with), "{without}");
    }
}

/// routine `s.mutate`의 간선(정렬한 `종류 대상`)과 분석 상태.
fn routine_edges_and_state(body: &str, dialect: &str) -> (Vec<String>, AnalysisState) {
    let graph = scan_routine_kind(body, dialect, "sql", "procedure");
    let mut edges: Vec<String> = graph
        .edges()
        .iter()
        .filter(|edge| edge.from.as_str() == "s.mutate")
        .map(|edge| format!("{:?} {}", edge.kind, edge.to))
        .collect();
    edges.sort();
    (
        edges,
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
    )
}

/// 세미콜론 없는 T-SQL 조각에서 `END` 뒤의 다음 문장 머리(`UPDATE`·`SET`·`IF`·`DECLARE`·`EXEC`·CTE를 여는
/// `WITH`)는 그 `END`의 꼬리가 아니다. 이전에는 꼬리 한 단어로 먹어 그 문장을 미추출로 셌고(`EXEC(@s)`는 간선
/// 없이), 세미콜론을 넣은 같은 몸체와 결과가 달랐다. `ELSE`·`TRY`/`CATCH`처럼 꼬리로 먹던 단어는 그대로다.
#[test]
fn tsql_end_does_not_swallow_the_next_statement_head() {
    let declare = "DECLARE @n int; ";
    let folded = "DECLARE @s nvarchar(100) = N'UPDATE audit SET name = 1'; ";
    for (without, with, expected) in [
        (
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x' END UPDATE orders SET amount = 0",
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x'; END; UPDATE orders SET amount = 0;",
            "Writes s.orders",
        ),
        (
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x' END SET @n = (SELECT count(*) FROM orders)",
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x'; END; SET @n = (SELECT count(*) FROM orders);",
            "Reads s.orders",
        ),
        (
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x' END IF @n = 2 BEGIN UPDATE orders SET amount = 0 END",
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x'; END; IF @n = 2 BEGIN UPDATE orders SET amount = 0; END;",
            "Writes s.orders",
        ),
        (
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x' END DECLARE @m int = (SELECT count(*) FROM orders)",
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x'; END; DECLARE @m int = (SELECT count(*) FROM orders);",
            "Reads s.orders",
        ),
        (
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x' END WITH big AS (SELECT id FROM orders) SELECT id FROM big",
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x'; END; WITH big AS (SELECT id FROM orders) SELECT id FROM big;",
            "Reads s.orders",
        ),
        (
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x' END ELSE BEGIN UPDATE customers SET name = 'y' END",
            "IF @n = 1 BEGIN UPDATE audit SET name = 'x'; END ELSE BEGIN UPDATE customers SET name = 'y'; END;",
            "Writes s.customers",
        ),
        (
            "BEGIN TRY UPDATE audit SET name = 'x' END TRY BEGIN CATCH UPDATE orders SET amount = 0 END CATCH",
            "BEGIN TRY UPDATE audit SET name = 'x'; END TRY BEGIN CATCH UPDATE orders SET amount = 0; END CATCH;",
            "Writes s.orders",
        ),
    ] {
        let (without, with) = (format!("{declare}{without}"), format!("{declare}{with}"));
        let facts = routine_edges_and_state(&without, "sqlserver");
        assert!(facts.0.iter().any(|edge| edge == expected), "{without}: {facts:?}");
        assert_eq!(facts, routine_edges_and_state(&with, "sqlserver"), "{without}");
    }
    // 블록 뒤의 `EXEC(@s)`는 블록 안에서 바꾼 값으로 접힌다(세미콜론 판과 같다).
    let without = format!("{folded}BEGIN SET @s = N'UPDATE orders SET amount = 1' END EXEC(@s)");
    let with = format!("{folded}BEGIN SET @s = N'UPDATE orders SET amount = 1'; END; EXEC(@s);");
    let facts = routine_edges_and_state(&without, "sqlserver");
    assert!(
        facts.0.iter().any(|edge| edge == "Writes s.orders"),
        "{facts:?}"
    );
    assert!(
        !facts.0.iter().any(|edge| edge == "Writes s.audit"),
        "{facts:?}"
    );
    assert_eq!(facts, routine_edges_and_state(&with, "sqlserver"));
    // 이어진 블록의 `BEGIN`은 이전처럼 꼬리로 먹는다 — 따로 보내면 제어 흐름으로 보아 접던 값을 버린다.
    let blocks =
        format!("{folded}BEGIN SET @s = N'UPDATE orders SET amount = 1' END BEGIN EXEC(@s) END");
    let facts = routine_edges_and_state(&blocks, "sqlserver");
    assert!(
        facts.0.iter().any(|edge| edge == "Writes s.orders"),
        "{facts:?}"
    );
    // 문장마다 `;`가 있는 방언의 `END IF`·`END LOOP`는 이전처럼 꼬리다(절차형 추출기로 가는 몸체).
    for (dialect, language, body) in [
        (
            "postgres",
            "plpgsql",
            "BEGIN IF true THEN UPDATE audit SET name = 'x'; END IF; UPDATE orders SET amount = 0; END",
        ),
        (
            "postgres",
            "plpgsql",
            "BEGIN LOOP UPDATE audit SET name = 'x'; EXIT; END LOOP; UPDATE orders SET amount = 0; END",
        ),
        (
            "mysql",
            "sql",
            "BEGIN SET @v = 1; IF @v = 1 THEN UPDATE audit SET name = 'x'; END IF; UPDATE orders SET amount = 0; END",
        ),
    ] {
        let graph = scan_routine_kind(body, dialect, language, "procedure");
        let detail = format!("{dialect}: {:?} {:?}", graph.edges(), graph.limitations());
        assert!(edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes), "{detail}");
        assert!(edge(&graph, "s.mutate", "s.orders", EdgeKind::Writes), "{detail}");
        assert!(
            !graph.limitations().iter().any(|note| note.contains("not extracted")),
            "{detail}"
        );
    }
}

/// T-SQL `OPEN 커서`는 커서 이름 하나로 끝난다(FOR 절이 없다). 이전에는 PL/pgSQL `OPEN c FOR <질의>`처럼 조각
/// 끝까지를 커서 질의로 읽어, 세미콜론 없이 이어진 `FETCH … UPDATE orders …`가 소리 없이 빠진 채 `complete`였다.
/// `FETCH … INTO @s`가 제 갈래를 거쳐 `@s`의 옛 값을 버리므로 뒤 `EXEC(@s)`를 옛 값으로 접지 않는다.
/// 키를 여는 `OPEN SYMMETRIC KEY …`는 다음 문장 머리에서 끝난다. PL/pgSQL·PL/SQL의 `OPEN … FOR`는 이전과 같다.
#[test]
fn tsql_open_ends_at_the_cursor_name() {
    for (without, with, expected) in [
        (
            "DECLARE @id int; DECLARE c CURSOR FOR SELECT id FROM audit; OPEN c FETCH NEXT FROM c INTO @id UPDATE orders SET amount = 0 WHERE id = @id",
            "DECLARE @id int; DECLARE c CURSOR FOR SELECT id FROM audit; OPEN c; FETCH NEXT FROM c INTO @id; UPDATE orders SET amount = 0 WHERE id = @id;",
            Some("Writes s.orders"),
        ),
        (
            "DECLARE c CURSOR GLOBAL FOR SELECT id FROM audit; OPEN GLOBAL c UPDATE orders SET amount = 0",
            "DECLARE c CURSOR GLOBAL FOR SELECT id FROM audit; OPEN GLOBAL c; UPDATE orders SET amount = 0;",
            Some("Writes s.orders"),
        ),
        // 이름이 `global`인 커서 — 뒤가 문장 머리면 GLOBAL은 범위 한정자가 아니라 이름이다.
        (
            "DECLARE global CURSOR FOR SELECT id FROM audit; OPEN global UPDATE orders SET amount = 0",
            "DECLARE global CURSOR FOR SELECT id FROM audit; OPEN global; UPDATE orders SET amount = 0;",
            Some("Writes s.orders"),
        ),
        (
            "DECLARE @c CURSOR; SET @c = CURSOR FOR SELECT id FROM audit; OPEN @c UPDATE orders SET amount = 0",
            "DECLARE @c CURSOR; SET @c = CURSOR FOR SELECT id FROM audit; OPEN @c; UPDATE orders SET amount = 0;",
            Some("Writes s.orders"),
        ),
        (
            "DECLARE [my c] CURSOR FOR SELECT id FROM audit; OPEN [my c] UPDATE orders SET amount = 0",
            "DECLARE [my c] CURSOR FOR SELECT id FROM audit; OPEN [my c]; UPDATE orders SET amount = 0;",
            Some("Writes s.orders"),
        ),
        (
            "DECLARE @n int; OPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE cert UPDATE orders SET amount = 0",
            "DECLARE @n int; OPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE cert; UPDATE orders SET amount = 0;",
            Some("Writes s.orders"),
        ),
        (
            "DECLARE @s nvarchar(100) = N'UPDATE audit SET name = 1'; DECLARE c CURSOR FOR SELECT name FROM customers; OPEN c FETCH NEXT FROM c INTO @s EXEC(@s)",
            "DECLARE @s nvarchar(100) = N'UPDATE audit SET name = 1'; DECLARE c CURSOR FOR SELECT name FROM customers; OPEN c; FETCH NEXT FROM c INTO @s; EXEC(@s);",
            None,
        ),
    ] {
        let facts = routine_edges_and_state(without, "sqlserver");
        match expected {
            Some(expected) => {
                assert!(facts.0.iter().any(|edge| edge == expected), "{without}: {facts:?}")
            }
            // `@s`의 값은 커서 행이라 모른다 — 동적 SQL은 미추출로 남는다.
            None => assert_eq!(facts.1, AnalysisState::Partial, "{without}: {facts:?}"),
        }
        assert!(!facts.0.iter().any(|edge| edge == "Writes s.audit"), "{without}: {facts:?}");
        assert_eq!(facts, routine_edges_and_state(with, "sqlserver"), "{without}");
    }
    for (dialect, language, body) in [
        (
            "postgres",
            "plpgsql",
            "DECLARE c refcursor; BEGIN OPEN c FOR SELECT id FROM orders; UPDATE audit SET name = 'x'; END",
        ),
        (
            "oracle",
            "plsql",
            "IS c SYS_REFCURSOR; BEGIN OPEN c FOR SELECT id FROM orders; UPDATE audit SET name = 'x'; END;",
        ),
    ] {
        let graph = scan_routine_kind(body, dialect, language, "procedure");
        assert!(
            edge(&graph, "s.mutate", "s.orders", EdgeKind::Reads),
            "{dialect}: {:?}",
            graph.edges()
        );
        assert!(
            edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
            "{dialect}: {:?}",
            graph.edges()
        );
    }
    // PL/SQL 매개변수 커서 `OPEN c(1)`의 인자는 다음 문장이 아니다.
    let graph = scan_routine_kind(
        "IS CURSOR c(p NUMBER) IS SELECT id FROM orders WHERE id = p; BEGIN OPEN c(1); UPDATE audit SET name = 'x'; CLOSE c; END;",
        "oracle",
        "plsql",
        "procedure",
    );
    assert!(
        edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
        "{:?}",
        graph.edges()
    );
    assert!(
        !graph
            .limitations()
            .iter()
            .any(|note| note.contains("not extracted")),
        "{:?}",
        graph.limitations()
    );
}

/// T-SQL routine 몸체는 문장 목록이라 `BEGIN … END`는 그 안의 블록 하나일 뿐이다. 첫 `BEGIN`과 마지막 `END` 사이만
/// 벗겨 읽던 몸체는 그 밖의 문장(아래 `UPDATE audit`·`UPDATE orders` 등)을 소리 없이 버리고 `complete`였다. 읽은
/// 간선은 그대로 두고 블록 밖을 분석하지 않았다고 부분 분석으로 알린다. 블록이 정의문 머리 뒤 몸체 전체를 감싸면
/// (주석, 문자열 속 `END`, `TRY`·`CATCH` 꼬리 포함) 이전처럼 `complete`이고, 다른 방언은 같은 몸체에서도 이 진단을
/// 내지 않는다.
#[test]
fn tsql_routine_text_outside_the_block_is_not_claimed_complete() {
    const OUTSIDE: &str =
        "T-SQL body text before the first BEGIN or after the last END was not analyzed";
    let outcome = |body: &str, dialect: &str, kind: &str| {
        let graph = scan_routine_kind(body, dialect, "sql", kind);
        let analysis = graph.analysis()[&VertexId::from_raw("s.mutate")].clone();
        let flagged = analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "SG_BODY_PARTIAL" && diagnostic.message.starts_with(OUTSIDE)
        });
        (graph, analysis.state, flagged)
    };
    let mixed = "UPDATE orders SET amount = 1; BEGIN UPDATE audit SET name = 'x'; END; UPDATE customers SET name = 'y';";
    for (body, kept) in [
        (
            "CREATE PROCEDURE s.mutate @id int AS\nUPDATE audit SET name = 'x' WHERE id = @id;\nIF @@ROWCOUNT = 0\nBEGIN\n  UPDATE customers SET name = 'y';\nEND\nUPDATE orders SET amount = 1;",
            "s.customers",
        ),
        (mixed, "s.audit"),
        (
            "CREATE PROCEDURE s.mutate AS\nBEGIN\n  UPDATE audit SET name = 'x'\nEND\nUPDATE orders SET amount = 1",
            "s.audit",
        ),
        (
            "CREATE PROCEDURE s.mutate @id int AS\nUPDATE orders SET amount = 1;\nBEGIN\n  UPDATE audit SET name = 'x';\nEND",
            "s.audit",
        ),
    ] {
        let (graph, state, flagged) = outcome(body, "sqlserver", "procedure");
        assert!(
            edge(&graph, "s.mutate", kept, EdgeKind::Writes),
            "{body}: {:?}",
            graph.edges()
        );
        assert_eq!(state, AnalysisState::Partial, "{body}");
        assert!(flagged, "{body}: {:?}", graph.analysis());
    }
    for (body, kind) in [
        (
            "CREATE PROCEDURE s.mutate @id int = 1 AS /* c */\nBEGIN\n  UPDATE audit SET name = 'END x';\nEND; -- tail",
            "procedure",
        ),
        (
            "CREATE PROCEDURE s.mutate AS\nBEGIN TRY\n  UPDATE audit SET name = 'x';\nEND TRY\nBEGIN CATCH\n  UPDATE orders SET amount = 0;\nEND CATCH",
            "procedure",
        ),
        (
            "CREATE FUNCTION s.mutate() RETURNS INT AS\nBEGIN\n  RETURN (SELECT count(*) FROM orders);\nEND",
            "function",
        ),
        // 마지막 `END CATCH`의 꼬리 — 블록을 벗긴 몸체를 문장 파서가 끝까지 읽는다.
        (
            "CREATE PROCEDURE s.mutate AS\nBEGIN\n  UPDATE audit SET name = 'x';\nEND;\nBEGIN TRY\n  UPDATE orders SET amount = 0;\nEND TRY\nBEGIN CATCH\n  UPDATE customers SET name = 'y';\nEND CATCH",
            "procedure",
        ),
    ] {
        let (graph, state, flagged) = outcome(body, "sqlserver", kind);
        assert!(!flagged, "{body}: {:?}", graph.analysis());
        assert_eq!(state, AnalysisState::Complete, "{body}: {:?}", graph.analysis());
    }
    for dialect in ["mysql", "postgres", "oracle"] {
        let (graph, _, flagged) = outcome(mixed, dialect, "procedure");
        assert!(!flagged, "{dialect}: {:?}", graph.analysis());
    }
}

/// MySQL·MariaDB·SQL Server·SQLite의 routine 몸체는 `AS` 뒤 문자열로 감싸이지 않는다. 그 방언들이 받는 문자열
/// 별칭(`AS 'total'`)을 PostgreSQL `AS '…'` 몸체처럼 꺼내, routine 전체가 별칭 한 단어로 바뀌고 간선 없이
/// `body parse failed`였다. PostgreSQL의 문자열 몸체는 이전처럼 꺼낸다.
#[test]
fn string_column_aliases_do_not_replace_the_routine_body() {
    for (dialect, body) in [
        (
            "mysql",
            "BEGIN UPDATE audit SET name = 'x'; SELECT COUNT(*) AS 'total' FROM orders; END",
        ),
        (
            "mariadb",
            "BEGIN UPDATE audit SET name = 'x'; SELECT COUNT(*) AS 'total' FROM orders; END",
        ),
        (
            "sqlserver",
            "CREATE PROCEDURE s.mutate AS\nBEGIN\n  UPDATE audit SET name = 'x';\n  SELECT id AS 'ident' FROM orders;\nEND",
        ),
        // 절차형 추출기로 가는 몸체(`DECLARE`)도 같다.
        (
            "sqlserver",
            "CREATE PROCEDURE s.mutate AS\nBEGIN\n  DECLARE @n int;\n  UPDATE audit SET name = 'x';\n  SELECT id AS 'ident' FROM orders;\nEND",
        ),
        (
            "sqlite",
            "UPDATE audit SET name = (SELECT COUNT(*) AS 'n' FROM orders)",
        ),
    ] {
        let graph = scan_routine_kind(body, dialect, "sql", "procedure");
        let detail = format!("{dialect}: {:?} {:?}", graph.edges(), graph.limitations());
        assert!(edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes), "{detail}");
        assert!(edge(&graph, "s.mutate", "s.orders", EdgeKind::Reads), "{detail}");
        assert_eq!(
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
    }
    let graph = scan_routine_kind(
        "CREATE FUNCTION s.mutate() RETURNS int LANGUAGE sql AS 'SELECT count(*) FROM orders'",
        "postgres",
        "sql",
        "function",
    );
    assert!(
        edge(&graph, "s.mutate", "s.orders", EdgeKind::Reads),
        "{:?}",
        graph.edges()
    );
}

/// 건너뛴 절에 대입이 있으면(`GET DIAGNOSTICS @s = ROW_COUNT`) 그 변수의 옛 값으로 뒤 동적 SQL을 접지
/// 않는다. 접으면 실행되지 않는 `UPDATE audit`가 거짓 쓰기가 된다.
#[test]
fn a_skipped_clause_with_an_assignment_forgets_variable_values() {
    let graph = scan_routine_kind(
        "DECLARE @s nvarchar(100) = N'UPDATE audit SET name = 1'; GET DIAGNOSTICS @s = ROW_COUNT EXEC(@s)",
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(!touches(&graph, "s.mutate", "s.audit"), "{detail}");
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Partial,
        "{detail}"
    );
}

/// 끝에 `END` 하나만 남은 몸체는 이전처럼 그 END를 버린다. SQLite·MySQL은 `SELECT id FROM audit end`의
/// `end`를 별칭으로 받는다(SQLite 3.53.1에서 그 뷰를 만들고 읽었다). END를 `COMMIT` 문장으로 붙이면
/// 뷰는 "exactly one query"가 아니라며 간선을 잃고, 쿼리 파일은 SELECT가 아니게 되어 컬럼 읽기를 잃었다.
#[test]
fn a_lone_trailing_end_is_dropped_for_views_and_query_files() {
    let view = scan_with_dialect("SELECT id FROM orders end", &["id"], "sqlite");
    let detail = format!("{:?} {:?}", view.edges(), view.analysis());
    complete(&view);
    assert!(
        edge(&view, "s.v", "s.orders.id", EdgeKind::Reads),
        "{detail}"
    );
    let query = scan_routine_kind("SELECT id FROM audit end", "mysql", "sql", "query");
    let detail = format!("{:?} {:?}", query.edges(), query.analysis());
    assert!(
        edge(&query, "s.mutate", "s.audit.id", EdgeKind::Reads),
        "{detail}"
    );
}

/// 다중 테이블 DELETE의 대상이 별칭과 대소문자만 다르면 문서가 증명하는 별칭 규칙을 따른다. MySQL은
/// 별칭을 `table_alias_charset`으로 비교하고(`multi_delete_table_match`), 그 charset은
/// `lower_case_table_names`=0이면 바이트 비교, 1·2면 대소문자 무시다(`mysqld.cc`). 0에서 `DELETE FROM C
/// USING audit c`는 `ER_UNKNOWN_TABLE`로 실패해 쓰기가 없다. 규칙을 모르는 문서(lctn 없음, SQL Server —
/// CTE 이름과 같이 어느 collation이 별칭을 비교하는지 문서가 없다, MariaDB)는 쓰기를 남기되 진단과
/// 부분 분석이다. 이전에는 어느 문서에서나 대소문자를 무시해 `complete`로 풀었다.
#[test]
fn delete_alias_case_follows_the_documented_alias_rule() {
    let bodies = [
        "DELETE FROM C USING audit c JOIN orders o ON c.id = o.id",
        "DELETE C FROM audit c JOIN orders o ON c.id = o.id",
    ];
    let mutate = VertexId::from_raw("s.mutate");
    for body in bodies {
        let sensitive = scan_routine_in_context(
            body,
            "mysql",
            "sql",
            "procedure",
            Some(naming_context(None, Some(0))),
        );
        let detail = format!("{body}: {:?}", sensitive.edges());
        assert!(
            !edge(&sensitive, "s.mutate", "s.audit", EdgeKind::Writes),
            "{detail}"
        );
        assert_eq!(
            sensitive.analysis()[&mutate].state,
            AnalysisState::Partial,
            "{detail}"
        );
        for lctn in [1, 2] {
            let folded = scan_routine_in_context(
                body,
                "mysql",
                "sql",
                "procedure",
                Some(naming_context(None, Some(lctn))),
            );
            let detail = format!(
                "{body} {lctn}: {:?} {:?}",
                folded.edges(),
                folded.limitations()
            );
            assert!(
                edge(&folded, "s.mutate", "s.audit", EdgeKind::Writes),
                "{detail}"
            );
            assert_eq!(
                folded.analysis()[&mutate].state,
                AnalysisState::Complete,
                "{detail}"
            );
        }
        let mut unknown_rules = vec![
            ("mysql", None),
            ("mariadb", Some(naming_context(None, Some(1)))),
        ];
        // USING 형식은 MySQL·MariaDB 문법이다. SQL Server는 대상 목록 형식만 쓴다.
        if body.starts_with("DELETE C FROM") {
            unknown_rules.extend([
                (
                    "sqlserver",
                    Some(naming_context(Some("Latin1_General_CS_AS"), None)),
                ),
                (
                    "sqlserver",
                    Some(naming_context(Some("SQL_Latin1_General_CP1_CI_AS"), None)),
                ),
                ("sqlserver", None),
            ]);
        }
        for (dialect, context) in unknown_rules {
            let unknown =
                scan_routine_in_context(body, dialect, "sql", "procedure", context.clone());
            let detail = format!("{body} {dialect} {context:?}: {:?}", unknown.analysis());
            assert!(
                edge(&unknown, "s.mutate", "s.audit", EdgeKind::Writes),
                "{detail}"
            );
            let analysis = &unknown.analysis()[&mutate];
            assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
            assert!(
                analysis.diagnostics.iter().any(|diagnostic| diagnostic.code
                    == "SG_DML_TARGET_SHAPE"
                    && diagnostic.message.starts_with(
                        "DML target 'C' names the alias 'c' only if letter case is ignored"
                    )),
                "{detail}"
            );
        }
    }
    // 철자가 같은 별칭은 어느 규칙에서나 그 관계다.
    let exact = scan_routine_in_context(
        "DELETE FROM c USING audit c JOIN orders o ON c.id = o.id",
        "mysql",
        "sql",
        "procedure",
        Some(naming_context(None, Some(0))),
    );
    assert!(edge(&exact, "s.mutate", "s.audit", EdgeKind::Writes));
    assert_eq!(exact.analysis()[&mutate].state, AnalysisState::Complete);
}

/// 무시하는 문장을 다음 문장 머리에서 끊는 일은 세미콜론을 생략할 수 있는 T-SQL에서만 한다. 추출기의
/// 인용 스캐너는 MySQL의 `\'` 이스케이프와 `#` 주석을 모르므로, MySQL·MariaDB에서 끊으면 문자열이나
/// 주석 속 `delete from audit`·`update audit`가 문장이 되어 거짓 쓰기가 생겼다(bc223db). 이 몸체들은
/// main과 같이 간선이 없어야 한다.
#[test]
fn skipped_statements_are_not_cut_outside_tsql() {
    let bodies = [
        "BEGIN\n IF p_force = 0 THEN\n SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Can\\'t delete from audit; it\\'s immutable';\n END IF;\nEND",
        "BEGIN\n IF p_force = 0 THEN\n SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'You can\\'t truncate audit; it\\'s protected';\n END IF;\nEND",
        "BEGIN\n SIGNAL SQLSTATE '45000' # callers update audit themselves\n SET MESSAGE_TEXT = 'x';\nEND",
        "BEGIN\n SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'x\\' update audit set name = 1 #\\'';\nEND",
        "BEGIN\n SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Don\\'t update orders now, it\\'s frozen';\nEND",
    ];
    for dialect in ["mysql", "mariadb"] {
        for body in bodies {
            let graph = scan_routine_kind(body, dialect, "sql", "procedure");
            let detail = format!(
                "{dialect} {body:?}: {:?} {:?}",
                graph.edges(),
                graph.limitations()
            );
            assert!(!touches(&graph, "s.mutate", "s.audit"), "{detail}");
            assert!(!touches(&graph, "s.mutate", "s.orders"), "{detail}");
            assert_ne!(
                graph.analysis()[&VertexId::from_raw("s.mutate")].state,
                AnalysisState::Complete,
                "{detail}"
            );
        }
    }
}

/// 건너뛴 절의 대입은 괄호 안에 있어도(`WAITFOR (RECEIVE … @s = message_body …)`), 절이 `;`에서 끝나도
/// (`GET DIAGNOSTICS @s = ROW_COUNT;`) 그 변수의 추적 값을 버린다. `PRINT 'x' SET @s = …`의 `SET`은 다음
/// 문장이라 그 값을 추적한다. 이전에는 세 경우 모두 옛 값 `UPDATE audit`로 `EXEC(@s)`를 접어 거짓
/// `writes audit`·`complete`였다. `SET @s += …`는 이어 붙인 값을 추적한다.
#[test]
fn skipped_clause_assignments_never_fold_a_stale_value() {
    let prefix = "DECLARE @s nvarchar(max) = N'UPDATE audit SET name = 1'; ";
    for clause in [
        "WAITFOR (RECEIVE TOP(1) @s = message_body FROM q), TIMEOUT 1000 EXEC(@s)",
        "WAITFOR (RECEIVE TOP(1) @s = message_body FROM q), TIMEOUT 1000; EXEC(@s);",
        "GET DIAGNOSTICS @s = ROW_COUNT; EXEC(@s);",
        "SET @s -= 1; EXEC(@s);",
        // 주석이 `=` 앞에 있어도 왼쪽 이름을 찾는다.
        "WAITFOR (RECEIVE TOP(1) @s /*x*/ = message_body FROM q), TIMEOUT 5 EXEC(@s)",
        // 왼쪽이 이름이 아니면 어느 변수인지 모르므로 모두 버린다.
        "GET DIAGNOSTICS (@s) = ROW_COUNT; EXEC(@s);",
    ] {
        let graph = scan_routine_kind(
            &format!("{prefix}{clause}"),
            "sqlserver",
            "sql",
            "procedure",
        );
        let detail = format!("{clause}: {:?} {:?}", graph.edges(), graph.limitations());
        assert!(!touches(&graph, "s.mutate", "s.audit"), "{detail}");
        assert_eq!(
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Partial,
            "{detail}"
        );
    }
    // 주석을 건너뛰고 찾은 이름(@s)만 버린다 — 다른 변수(@t)의 접기는 그대로다.
    let targeted = scan_routine_kind(
        &format!(
            "{prefix}DECLARE @t nvarchar(max) = N'UPDATE orders SET amount = 1'; WAITFOR (RECEIVE TOP(1) @s /*x*/ = message_body FROM q), TIMEOUT 5 EXEC(@t)"
        ),
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?} {:?}", targeted.edges(), targeted.limitations());
    assert!(
        edge(&targeted, "s.mutate", "s.orders", EdgeKind::Writes),
        "{detail}"
    );
    assert!(!touches(&targeted, "s.mutate", "s.audit"), "{detail}");
    let reassigned = scan_routine_kind(
        &format!("{prefix}PRINT 'x' SET @s = N'UPDATE orders SET amount = 1'; EXEC(@s)"),
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?}", reassigned.edges());
    assert!(
        edge(&reassigned, "s.mutate", "s.orders", EdgeKind::Writes),
        "{detail}"
    );
    assert!(!touches(&reassigned, "s.mutate", "s.audit"), "{detail}");
    let appended = scan_routine_kind(
        &format!("{prefix}SET @s += N' WHERE 1 = 0'; EXEC(@s)"),
        "sqlserver",
        "sql",
        "procedure",
    );
    let detail = format!("{:?} {:?}", appended.edges(), appended.limitations());
    assert!(
        edge(&appended, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
    assert_eq!(
        appended.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Complete,
        "{detail}"
    );
}

/// MySQL `lower_case_table_names=0`에서 대상이 별칭과 대소문자만 다르면 MySQL은 그 문장을
/// `ER_UNKNOWN_TABLE`로 거절한다. 카탈로그에 그 철자의 테이블 `C`가 있어도(lctn 0에서는 `c`와 함께 있을
/// 수 있다) `C`에 쓰지 않는다. FROM 목록에 별칭 없는 `C`가 있으면 그 테이블이 대상이다.
#[test]
fn case_sensitive_alias_mismatch_writes_no_catalog_table() {
    let scan = |body: &str| {
        scan_routine_with_objects(
            body,
            "mysql",
            "sql",
            "procedure",
            Some(naming_context(None, Some(0))),
            vec![object("c", &["id"]), object("C", &["id"])],
        )
    };
    for body in [
        "DELETE C FROM audit c WHERE c.id = 1",
        "DELETE FROM C USING audit c WHERE c.id = 1",
    ] {
        let graph = scan(body);
        let detail = format!("{body}: {:?} {:?}", graph.edges(), graph.analysis());
        assert!(
            !edge(&graph, "s.mutate", "s.C", EdgeKind::Writes),
            "{detail}"
        );
        assert!(
            !edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
            "{detail}"
        );
        let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
        assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        assert!(
            analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "SG_DML_TARGET_SHAPE"
                    && diagnostic
                        .message
                        .starts_with("DML target 'C' matches the alias 'c' only by letter case")),
            "{detail}"
        );
    }
    let joined = scan("DELETE C FROM audit c JOIN C ON C.id = c.id");
    let detail = format!("{:?}", joined.edges());
    assert!(
        edge(&joined, "s.mutate", "s.C", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        !edge(&joined, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
}

/// 뷰·쿼리 파일에서 `end` 별칭 뒤에 절이 이어지면(SQLite 3.53.1은 세 뷰를 모두 만든다) sqlparser는 그
/// `END`에서 멈춘다. 그 앞 SELECT의 간선은 남기고 뒤 절을 분석하지 않았다고 부분 분석으로 표시한다.
/// main은 뒤를 소리 없이 버렸고(`complete`일 수 있었다), bc223db는 파싱 실패로 간선을 모두 잃었다.
#[test]
fn an_end_alias_followed_by_clauses_keeps_the_select_edges() {
    for body in [
        "select id from orders end order by id",
        "select id from orders end where end.id = 1",
        "select end.id from orders end join customers o on o.id = end.id",
    ] {
        let view = scan_with_dialect(body, &["id"], "sqlite");
        let detail = format!("{body}: {:?} {:?}", view.edges(), view.analysis());
        assert!(edge(&view, "s.v", "s.orders", EdgeKind::Reads), "{detail}");
        let analysis = &view.analysis()[&VertexId::from_raw("s.v")];
        assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        assert!(
            analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "SG_BODY_PARTIAL"
                    && diagnostic
                        .message
                        .starts_with("SQL text from a bare END onward was not parsed")),
            "{detail}"
        );
    }
    let query = scan_routine_kind(
        "select id from audit end where end.id = 1",
        "mysql",
        "sql",
        "query",
    );
    let detail = format!("{:?} {:?}", query.edges(), query.analysis());
    assert!(
        edge(&query, "s.mutate", "s.audit.id", EdgeKind::Reads),
        "{detail}"
    );
}

/// T-SQL에서 끊은 자리의 읽기·쓰기는 세미콜론 판과 같다 — `EXECUTE proc` 인자 뒤 문장, `PRINT`의 하위 질의,
/// 그리고 `WITH cte AS (…)`로 여는 문장. 이전에는 `PRINT 'x' WITH cte AS (…) INSERT …`가 `WITH`를 `PRINT`의
/// 절로 버려 실제 테이블 `cte`를 읽는 거짓 간선을 냈다.
#[test]
fn tsql_cut_points_match_the_semicolon_version() {
    let facts = |body: &str| {
        let graph = scan_routine_with_objects(
            body,
            "sqlserver",
            "sql",
            "procedure",
            None,
            vec![object("cte", &["id"])],
        );
        let mut edges: Vec<String> = graph
            .edges()
            .iter()
            .filter(|edge| edge.from.as_str() == "s.mutate")
            .map(|edge| format!("{:?} {}", edge.kind, edge.to))
            .collect();
        edges.sort();
        edges
    };
    for (without, with) in [
        (
            "EXECUTE log_error @code = 1 INSERT INTO orders (id) VALUES (1)",
            "EXECUTE log_error @code = 1; INSERT INTO orders (id) VALUES (1)",
        ),
        (
            "DECLARE @n int; PRINT (SELECT COUNT(*) FROM audit) UPDATE orders SET amount = 1",
            "DECLARE @n int; PRINT (SELECT COUNT(*) FROM audit); UPDATE orders SET amount = 1",
        ),
    ] {
        assert_eq!(facts(without), facts(with), "{without}");
        assert!(facts(without)
            .iter()
            .any(|edge| edge.starts_with("Writes s.orders")));
    }
    let printed =
        facts("DECLARE @n int; PRINT (SELECT COUNT(*) FROM audit) UPDATE orders SET amount = 1");
    assert!(
        printed.iter().any(|edge| edge == "Reads s.audit"),
        "{printed:?}"
    );
    let cte = facts(
        "PRINT 'x' WITH cte AS (SELECT id FROM audit) INSERT INTO orders (id) SELECT id FROM cte",
    );
    assert!(!cte.iter().any(|edge| edge.contains("s.cte")), "{cte:?}");
}

/// 세미콜론 없는 T-SQL에서 `SET`은 우변·설정 뒤에 붙은 다음 문장 머리에서 끝난다. 이전에는 `SET` 갈래가
/// 조각 끝까지를 우변으로 삼아, `PRINT 'x' SET @n = 1 UPDATE audit …`(5b11c7a에서 `SET`을 끊기 머리로 더한 뒤)와
/// 맨 앞의 `SET @n = 1 UPDATE audit …`(main부터)가 `UPDATE`를 소리 없이 잃고 `complete`였다. 세미콜론을 붙인
/// 같은 몸체와 같은 간선·상태여야 한다.
#[test]
fn set_statements_end_at_the_next_statement_head() {
    let facts = |body: &str| {
        let graph = scan_routine_kind(body, "sqlserver", "sql", "procedure");
        let mut edges: Vec<String> = graph
            .edges()
            .iter()
            .filter(|edge| edge.from.as_str() == "s.mutate")
            .map(|edge| format!("{:?} {}", edge.kind, edge.to))
            .collect();
        edges.sort();
        (
            edges,
            graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        )
    };
    for (without, with, written) in [
        (
            "DECLARE @n int; UPDATE orders SET amount = 1; PRINT 'x' SET @n = 1 UPDATE audit SET name = 'x'",
            "DECLARE @n int; UPDATE orders SET amount = 1; PRINT 'x'; SET @n = 1; UPDATE audit SET name = 'x'",
            "Writes s.audit",
        ),
        (
            "DECLARE @c int; EXEC mutate SET @c = 3 UPDATE audit SET name = 'x'",
            "DECLARE @c int; EXEC mutate; SET @c = 3; UPDATE audit SET name = 'x'",
            "Writes s.audit",
        ),
        (
            "BEGIN TRY UPDATE orders SET amount = 1 END TRY BEGIN CATCH PRINT ERROR_MESSAGE() SET @n = 1 INSERT INTO audit (id) VALUES (1) END CATCH",
            "BEGIN TRY UPDATE orders SET amount = 1; END TRY BEGIN CATCH PRINT ERROR_MESSAGE(); SET @n = 1; INSERT INTO audit (id) VALUES (1); END CATCH",
            "Writes s.audit",
        ),
        (
            "DECLARE @c int; PRINT 'x' SET NOCOUNT ON UPDATE orders SET amount = 1",
            "DECLARE @c int; PRINT 'x'; SET NOCOUNT ON; UPDATE orders SET amount = 1",
            "Writes s.orders",
        ),
        (
            "DECLARE @c int; PRINT 'x' SET XACT_ABORT ON INSERT INTO audit (id) VALUES (1)",
            "DECLARE @c int; PRINT 'x'; SET XACT_ABORT ON; INSERT INTO audit (id) VALUES (1)",
            "Writes s.audit",
        ),
        (
            "DECLARE @c int; WAITFOR DELAY '00:00:01' SET @c = 3 UPDATE audit SET name = 'x'",
            "DECLARE @c int; WAITFOR DELAY '00:00:01'; SET @c = 3; UPDATE audit SET name = 'x'",
            "Writes s.audit",
        ),
        (
            "DECLARE @n int; SET @n = 1 UPDATE audit SET name = 'x'",
            "DECLARE @n int; SET @n = 1; UPDATE audit SET name = 'x'",
            "Writes s.audit",
        ),
    ] {
        let (edges, state) = facts(without);
        assert!(edges.iter().any(|edge| edge == written), "{without}: {edges:?}");
        assert_eq!((edges, state), facts(with), "{without}");
    }
}

/// MySQL이 거절하는 DELETE(대상 하나가 lctn 0에서 별칭과 대소문자만 다름)는 문장 전체가 실행되지 않는다 —
/// 다른 대상(`o`)에도 쓰지 않고 그 문장의 읽기도 없다. 이전에는 거절된 대상만 빼고 `writes orders`와
/// `reads audit`를 남겼다.
#[test]
fn a_rejected_delete_records_no_writes_or_reads() {
    for body in [
        "DELETE C, o FROM audit c JOIN orders o ON o.id = c.id",
        "DELETE FROM C, o USING audit c JOIN orders o ON o.id = c.id",
        "DELETE C FROM audit c WHERE c.id IN (SELECT id FROM C)",
        // 절차형 추출로 가는 몸체도 거절 진단과 부분 분석을 남긴다(간선이 없어도 파싱 실패가 아니다).
        "BEGIN DECLARE x INT DEFAULT 1; DELETE C FROM audit c WHERE c.id = x; END",
    ] {
        let graph = scan_routine_with_objects(
            body,
            "mysql",
            "sql",
            "procedure",
            Some(naming_context(None, Some(0))),
            vec![object("c", &["id"]), object("C", &["id"])],
        );
        let detail = format!("{body}: {:?} {:?}", graph.edges(), graph.analysis());
        for target in ["s.audit", "s.orders", "s.C", "s.c"] {
            assert!(!touches(&graph, "s.mutate", target), "{detail}");
        }
        let analysis = &graph.analysis()[&VertexId::from_raw("s.mutate")];
        assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
        assert!(
            analysis.diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("rejects the whole statement (ER_UNKNOWN_TABLE)")),
            "{detail}"
        );
    }
}

/// MySQL USING 형식의 FROM 목록 별칭은 그 자리만 읽기에서 뺀다. 하위 질의가 같은 이름의 실제 테이블
/// `c`를 읽는 것은 읽기다.
#[test]
fn mysql_using_alias_exclusion_does_not_hide_a_real_table_read() {
    let graph = scan_routine_with_objects(
        "DELETE FROM c USING audit c WHERE c.id IN (SELECT id FROM c)",
        "mysql",
        "sql",
        "procedure",
        Some(naming_context(None, Some(0))),
        vec![object("c", &["id"])],
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        edge(&graph, "s.mutate", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
    assert!(edge(&graph, "s.mutate", "s.c", EdgeKind::Reads), "{detail}");
    assert!(
        !edge(&graph, "s.mutate", "s.c", EdgeKind::Writes),
        "{detail}"
    );
}

/// 앞에 WITH가 붙은 DELETE도 CTE 몸체가 읽는 컬럼을 남긴다. 읽기 경로로 보내던 때에는 그
/// 컬럼 읽기가 남았는데, DML 경로로 옮기면서 DELETE를 컬럼 분석기로 보내지 않아 사라졌다.
#[test]
fn with_prefixed_delete_keeps_the_cte_column_reads() {
    for (dialect, body, reads) in [
        (
            "postgres",
            "WITH x AS (SELECT id FROM audit) DELETE FROM audit WHERE id IN (SELECT id FROM x)",
            &["s.audit.id"][..],
        ),
        (
            "sqlserver",
            "WITH dupes AS (SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) AS rn FROM customers) DELETE FROM dupes WHERE rn > 1",
            &["s.customers.id", "s.customers.name"][..],
        ),
        (
            "mysql",
            "WITH c AS (SELECT id FROM orders) DELETE audit FROM audit JOIN c ON audit.id = c.id",
            &["s.orders.id"][..],
        ),
    ] {
        for kind in ["query", "procedure"] {
            let graph = scan_routine_kind(body, dialect, "sql", kind);
            let detail = format!("{dialect} {kind}: {:?} {:?}", graph.edges(), graph.limitations());
            for column in reads {
                assert!(
                    edge(&graph, "s.mutate", column, EdgeKind::Reads),
                    "{column}: {detail}"
                );
            }
        }
    }
}

/// PostgreSQL·SQLite에서 DML 대상은 같은 이름의 CTE가 있어도 실제 테이블이다(PostgreSQL 16·
/// SQLite 3.53.1로 확인: `WITH audit AS (SELECT … FROM staging) INSERT INTO audit … FROM audit`는
/// CTE 행을 실제 `audit`에 넣는다). 원천의 `audit`만 CTE다.
#[test]
fn postgres_and_sqlite_dml_targets_ignore_a_cte_of_the_same_name() {
    for dialect in ["postgres", "sqlite"] {
        let insert = scan_routine_kind(
            "WITH audit AS (SELECT id, name FROM customers) INSERT INTO audit (id, name) SELECT id, name FROM audit",
            dialect,
            "sql",
            "query",
        );
        let detail = format!(
            "{dialect} insert: {:?} {:?}",
            insert.edges(),
            insert.limitations()
        );
        assert_eq!(
            insert.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
        assert!(
            edge(&insert, "s.mutate", "s.audit", EdgeKind::Writes),
            "{detail}"
        );
        assert!(
            edge(
                &insert,
                "s.audit.name",
                "s.customers.name",
                EdgeKind::DerivesFrom
            ),
            "{detail}"
        );
        assert!(
            !edge(&insert, "s.mutate", "s.audit", EdgeKind::Reads),
            "{detail}"
        );

        let update = scan_routine_kind(
            "WITH audit AS (SELECT id, name FROM customers) UPDATE audit SET name = 'x'",
            dialect,
            "sql",
            "query",
        );
        let detail = format!(
            "{dialect} update: {:?} {:?}",
            update.edges(),
            update.limitations()
        );
        assert_eq!(
            update.analysis()[&VertexId::from_raw("s.mutate")].state,
            AnalysisState::Complete,
            "{detail}"
        );
        assert!(
            edge(&update, "s.mutate", "s.audit.name", EdgeKind::Writes),
            "{detail}"
        );
        assert!(
            !edge(&update, "s.mutate", "s.customers.name", EdgeKind::Writes),
            "{detail}"
        );
    }
}

/// 앞에 WITH가 붙은 DML의 `database.schema.table` 대상도 수집 DB와 같으면 로컬 쓰기다.
/// 3부분 쓰기 대상을 바깥 문장에서만 찾으면 이 쓰기가 빠진다.
#[test]
fn with_prefixed_insert_into_a_database_qualified_target_is_a_local_write() {
    let doc = CatalogDocument {
        context: Some(CollectionContext {
            source_id: "fixture".into(),
            database: Some("app".into()),
            schema_filter: None,
            catalog_complete: true,
            lower_case_table_names: None,
            collation: None,
        }),
        dependencies: Vec::new(),
        version: 1,
        dialect: "sqlserver".into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            objects: vec![
                object("customers", &["id", "name"]),
                object("audit", &["id", "name"]),
            ],
            routines: vec![case_routine(
                "load",
                "query",
                "WITH c AS (SELECT id, name FROM s.customers) INSERT INTO app.s.audit (id, name) SELECT id, name FROM c",
            )],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        edge(&graph, "s.load", "s.audit", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        edge(&graph, "s.load", "s.audit.name", EdgeKind::Writes),
        "{detail}"
    );
    assert!(
        !edge(&graph, "s.load", "s.audit", EdgeKind::Reads),
        "{detail}"
    );
    assert!(!has_diagnostic(&graph, "SG_CROSS_DATABASE"), "{detail}");
}

#[test]
fn temporal_self_update_is_partial_without_self_derives_edge() {
    let graph = scan_routine("UPDATE orders SET amount = amount + 1");
    assert!(edge(
        &graph,
        "s.mutate",
        "s.orders.amount",
        EdgeKind::Writes
    ));
    assert!(edge(&graph, "s.mutate", "s.orders.amount", EdgeKind::Reads));
    assert!(!edge(
        &graph,
        "s.orders.amount",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(has_diagnostic(&graph, "SG_TEMPORAL_SELF_LINEAGE"));
}

#[test]
fn control_flow_dml_is_explicitly_partial() {
    let graph =
        scan_routine("BEGIN IF amount > 0 THEN UPDATE orders SET amount = amount + 1; END IF; END");
    assert!(has_diagnostic(&graph, "SG_DML_CONTROL_FLOW"));
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Partial
    );
}

#[test]
fn straight_line_temp_ctas_collapses_to_real_sources() {
    let graph = scan_routine(
        "CREATE TEMP TABLE tmp AS SELECT id, name FROM customers; INSERT INTO audit (id, name) SELECT id, name FROM tmp",
    );
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(!graph.limitations().iter().any(|note| note.contains("tmp")));
    assert!(!graph.vertices().any(|vertex| vertex.name == "tmp"));
}

#[test]
fn straight_line_temp_insert_appends_sources_without_graph_writes() {
    let graph = scan_routine(
        "CREATE TEMP TABLE tmp AS SELECT id, name FROM customers; INSERT INTO tmp (id, name) SELECT id, customer_id FROM orders; INSERT INTO audit (id, name) SELECT id, name FROM tmp",
    );
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(&graph, "s.mutate", "s.customers", EdgeKind::Writes));
    assert!(!graph.limitations().iter().any(|note| note.contains("tmp")));
}

#[test]
fn temp_update_uses_local_column_identity_and_preserves_unmatched_rows() {
    let graph = scan_routine(
        "CREATE TEMP TABLE tmp AS SELECT id AS a, id AS b FROM orders; UPDATE tmp SET b = (SELECT id FROM customers LIMIT 1); INSERT INTO audit (id, name) SELECT a, b FROM tmp",
    );
    assert!(edge(
        &graph,
        "s.audit.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn unknown_dynamic_sql_invalidates_temp_lineage() {
    let graph = scan_routine_with(
        "BEGIN CREATE TEMP TABLE tmp AS SELECT id, name FROM customers; EXECUTE dynamic_sql_variable; INSERT INTO audit (id, name) SELECT id, name FROM tmp; END",
        "postgres",
        "plpgsql",
    );
    assert!(has_diagnostic(&graph, "SG_DML_TEMP_STATE"));
    assert!(!edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn drop_invalidates_temp_lineage_before_later_reads() {
    let graph = scan_routine(
        "CREATE TEMP TABLE tmp AS SELECT id, name FROM customers; DROP TABLE tmp; INSERT INTO audit (id, name) SELECT id, name FROM tmp",
    );
    assert!(has_diagnostic(&graph, "SG_DML_TEMP_STATE"));
    assert!(!edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn straight_line_temp_update_rebinds_without_writing_source_columns() {
    let graph = scan_routine(
        "CREATE TEMP TABLE tmp AS SELECT id, name FROM customers; UPDATE tmp SET name = name; INSERT INTO audit (id, name) SELECT id, name FROM tmp",
    );
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &graph,
        "s.mutate",
        "s.customers.name",
        EdgeKind::Writes
    ));
    assert!(!graph.limitations().iter().any(|note| note.contains("tmp")));
}

#[test]
fn unsupported_target_shape_is_partial_without_ghost_columns() {
    let graph = scan_routine("INSERT INTO audit SELECT * FROM orders");
    assert!(has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"));
    assert!(!graph
        .vertices()
        .any(|vertex| vertex.id.as_str().contains("sink")));
}

#[test]
fn implicit_insert_shape_is_partial_even_when_catalog_width_matches() {
    let graph = scan_routine("INSERT INTO audit SELECT id, name FROM customers");
    assert!(has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"));
    assert!(!edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Writes));
    assert!(!edge(
        &graph,
        "s.audit.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn unresolved_insert_target_name_does_not_shift_a_later_column() {
    let graph =
        scan_routine("INSERT INTO audit (missing_column, name) SELECT c.name FROM customers c");
    assert!(has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"));
    assert!(!edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes));
    assert!(!edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn unresolved_temp_insert_target_name_does_not_shift_a_later_column() {
    let graph = scan_routine(
        "CREATE TEMP TABLE tmp AS SELECT id, customer_id AS name FROM orders; INSERT INTO tmp (missing_column, name) SELECT name FROM customers; INSERT INTO audit (id, name) SELECT id, name FROM tmp",
    );
    assert!(has_diagnostic(&graph, "SG_DML_TARGET_SHAPE"));
    assert!(edge(
        &graph,
        "s.audit.name",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &graph,
        "s.audit.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn rewritten_procedural_sql_does_not_claim_raw_source_locations() {
    let graph = scan_routine_with(
        "CREATE FUNCTION mutate() RETURNS void AS $$ BEGIN INSERT INTO audit (id, name) SELECT id, name FROM customers; END $$ LANGUAGE plpgsql",
        "postgres",
        "plpgsql",
    );
    let origins: Vec<_> = graph
        .origins()
        .values()
        .flatten()
        .filter(|origin| origin.role.starts_with("dml-owner:"))
        .collect();
    assert!(!origins.is_empty());
    assert!(origins.iter().all(|origin| origin.location.is_none()));
}

#[test]
fn matching_database_qualified_relation_is_local_but_mismatch_is_partial() {
    fn graph(database: &str) -> Graph {
        let doc = CatalogDocument {
            context: Some(CollectionContext {
                source_id: "fixture".into(),
                database: Some("corpus".into()),
                schema_filter: Some(vec!["public".into()]),
                catalog_complete: true,
                lower_case_table_names: None,
                collation: None,
            }),
            dependencies: Vec::new(),
            version: 1,
            dialect: "postgres".into(),
            reader: "fixture".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "public".into(),
                objects: vec![object("orders", &["id", "amount"])],
                routines: vec![RoutineDoc {
                    name: "model".into(),
                    kind: "query".into(),
                    language: Some("sql".into()),
                    body: Some(format!(
                        "SELECT id, amount FROM {database}.\"public\".\"orders\""
                    )),
                    signature: None,
                    usage: None,
                    member_of: None,
                    source: Some("models/model.sql".into()),
                }],
            }],
        };
        let mut graph = schemagraph_source::graph::document_to_graph(&doc);
        let (_, notes) = super::enrich_from_document(&mut graph, &doc);
        for note in notes {
            graph.add_limitation(note);
        }
        graph
    }

    let matching = graph("\"corpus\"");
    assert!(edge(
        &matching,
        "public.model",
        "public.orders",
        EdgeKind::Reads
    ));
    assert!(edge(
        &matching,
        "public.model",
        "public.orders.id",
        EdgeKind::Reads
    ));
    assert!(!has_diagnostic(&matching, "SG_CROSS_DATABASE"));

    let folded = graph("Corpus");
    assert!(edge(
        &folded,
        "public.model",
        "public.orders.id",
        EdgeKind::Reads
    ));
    assert!(!has_diagnostic(&folded, "SG_CROSS_DATABASE"));

    let mismatched = graph("\"warehouse\"");
    assert!(!edge(
        &mismatched,
        "public.model",
        "public.orders",
        EdgeKind::Reads
    ));
    assert!(!edge(
        &mismatched,
        "public.model",
        "public.orders.id",
        EdgeKind::Reads
    ));
    assert!(has_diagnostic(&mismatched, "SG_CROSS_DATABASE"));
}

#[test]
fn generated_default_insert_shape_is_partial_without_guessed_writes() {
    let graph = scan_routine("INSERT INTO audit DEFAULT VALUES");
    assert!(has_diagnostic(&graph, "SG_DML_SOURCE_SHAPE"));
    assert!(!edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Writes));
}

#[test]
fn unqualified_column_and_predicate_have_distinct_lineage() {
    let g = scan(
        "SELECT amount AS total FROM orders WHERE customer_id IS NOT NULL",
        &["total"],
    );
    complete(&g);
    assert!(edge(
        &g,
        "s.v.total",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&g, "s.v", "s.orders.customer_id", EdgeKind::Reads));
    assert!(!edge(
        &g,
        "s.v.total",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(g.origins().values().flatten().any(|o| o.location.is_some()));
}

#[test]
fn ambiguous_unqualified_column_is_not_guessed() {
    let g = scan(
        "SELECT id FROM orders o JOIN customers c ON c.id=o.customer_id",
        &["id"],
    );
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(!edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(!edge(&g, "s.v.id", "s.customers.id", EdgeKind::DerivesFrom));
    assert!(g
        .analysis()
        .values()
        .flat_map(|a| &a.diagnostics)
        .any(|d| d.code == "SG_COLUMN_AMBIGUOUS"));
}

#[test]
fn wildcard_and_cte_column_aliases_preserve_position() {
    let g = scan(
        "WITH q(a,b,c) AS (SELECT * FROM orders) SELECT q.* FROM q",
        &["first", "second", "third"],
    );
    complete(&g);
    for (out, input) in [
        ("first", "id"),
        ("second", "customer_id"),
        ("third", "amount"),
    ] {
        assert!(edge(
            &g,
            &format!("s.v.{out}"),
            &format!("s.orders.{input}"),
            EdgeKind::DerivesFrom
        ));
    }
    assert!(!g.limitations().iter().any(|n| n.contains("s.q")));
}

#[test]
fn cte_shadows_table_but_schema_qualified_table_does_not() {
    let g=scan("WITH orders AS (SELECT name FROM customers) SELECT q.name,o.id FROM orders q CROSS JOIN s.orders o",&["name","id"]);
    complete(&g);
    assert!(edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
}

#[test]
fn correlated_subquery_keeps_inner_and_outer_aliases_separate() {
    let g=scan("SELECT o.id,(SELECT c.name FROM customers c WHERE c.id=o.customer_id) AS name FROM orders o",&["id","name"]);
    complete(&g);
    assert!(edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(&g, "s.v", "s.orders.customer_id", EdgeKind::Reads));
    assert!(!edge(&g, "s.v.id", "s.customers.id", EdgeKind::DerivesFrom));
}

#[test]
fn inner_alias_does_not_resolve_an_unknown_outer_column() {
    let g = scan(
        "SELECT o.name FROM orders o WHERE EXISTS (SELECT 1 FROM customers o)",
        &["name"],
    );
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(!edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn intersect_preserves_both_value_sources() {
    let graph = scan(
        "SELECT customer_id AS id FROM orders INTERSECT SELECT id FROM customers",
        &["id"],
    );
    complete(&graph);
    for source in ["s.orders.customer_id", "s.customers.id"] {
        assert!(edge(&graph, "s.v", source, EdgeKind::Reads));
        assert!(edge(&graph, "s.v.id", source, EdgeKind::DerivesFrom));
    }
    assert!(!edge(
        &graph,
        "s.v.id",
        "s.orders.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn union_collects_both_values_but_except_rhs_only_affects_rows() {
    let union = scan(
        "SELECT id FROM orders UNION ALL SELECT id FROM customers",
        &["id"],
    );
    complete(&union);
    assert!(edge(&union, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(edge(
        &union,
        "s.v.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
    let except = scan(
        "SELECT id FROM orders EXCEPT SELECT id FROM customers",
        &["id"],
    );
    complete(&except);
    assert!(edge(&except, "s.v", "s.customers.id", EdgeKind::Reads));
    assert!(!edge(
        &except,
        "s.v.id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn nested_join_keeps_each_qualified_relation() {
    let g = scan(
        "SELECT o.id,c.name FROM (orders o JOIN customers c ON o.customer_id=c.id)",
        &["id", "name"],
    );
    complete(&g);
    assert!(edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
    assert!(edge(
        &g,
        "s.v.name",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn join_using_merges_unqualified_key_and_preserves_qualified_keys() {
    let g = scan(
        "SELECT id,o.id,c.id FROM orders o JOIN customers c USING(id)",
        &["merged", "left_id", "right_id"],
    );
    complete(&g);
    for source in ["s.orders.id", "s.customers.id"] {
        assert!(edge(&g, "s.v.merged", source, EdgeKind::DerivesFrom));
    }
    assert!(!edge(
        &g,
        "s.v.left_id",
        "s.customers.id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn plain_left_and_explicit_inner_join_conditions_are_reads_not_value_lineage() {
    for join in ["JOIN", "INNER JOIN", "LEFT JOIN", "LEFT OUTER JOIN"] {
        let g = scan(
            &format!("SELECT o.amount FROM orders o {join} customers c ON o.customer_id=c.id"),
            &["amount"],
        );
        complete(&g);
        for input in ["s.orders.customer_id", "s.customers.id"] {
            assert!(edge(&g, "s.v", input, EdgeKind::Reads), "{join}: {input}");
            assert!(!edge(&g, "s.v.amount", input, EdgeKind::DerivesFrom));
        }
    }
}

#[test]
fn outer_join_using_lineage_follows_the_preserved_side() {
    for (join, source) in [
        ("LEFT JOIN", "s.orders.id"),
        ("RIGHT JOIN", "s.customers.id"),
        ("FULL JOIN", "both"),
    ] {
        let graph = scan(
            &format!(
                "SELECT id AS merged, o.id AS left_id, c.id AS right_id FROM orders o {join} customers c USING(id)"
            ),
            &["merged", "left_id", "right_id"],
        );
        complete(&graph);
        assert!(
            edge(&graph, "s.v.merged", "s.orders.id", EdgeKind::DerivesFrom)
                == (source != "s.customers.id")
        );
        assert!(
            edge(
                &graph,
                "s.v.merged",
                "s.customers.id",
                EdgeKind::DerivesFrom
            ) == (source != "s.orders.id")
        );
        assert!(edge(
            &graph,
            "s.v.left_id",
            "s.orders.id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(
            &graph,
            "s.v.right_id",
            "s.customers.id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(&graph, "s.v", "s.orders.id", EdgeKind::Reads));
        assert!(edge(&graph, "s.v", "s.customers.id", EdgeKind::Reads));
    }
}

#[test]
fn natural_outer_join_using_lineage_follows_the_preserved_side() {
    for (join, source) in [
        ("LEFT JOIN", "s.orders.id"),
        ("RIGHT JOIN", "s.customers.id"),
        ("FULL JOIN", "both"),
    ] {
        let graph = scan(
            &format!("SELECT id AS merged FROM orders o NATURAL {join} customers c"),
            &["merged"],
        );
        complete(&graph);
        assert!(
            edge(&graph, "s.v.merged", "s.orders.id", EdgeKind::DerivesFrom)
                == (source != "s.customers.id")
        );
        assert!(
            edge(
                &graph,
                "s.v.merged",
                "s.customers.id",
                EdgeKind::DerivesFrom
            ) == (source != "s.orders.id")
        );
        assert!(edge(&graph, "s.v", "s.orders.id", EdgeKind::Reads));
        assert!(edge(&graph, "s.v", "s.customers.id", EdgeKind::Reads));
    }
}

#[test]
fn sqlite_outer_join_using_keeps_joined_column_order() {
    for (join, source) in [
        ("LEFT JOIN", "s.orders.id"),
        ("RIGHT JOIN", "s.customers.id"),
        ("FULL JOIN", "both"),
    ] {
        let graph = scan_with_dialect(
            &format!("SELECT * FROM orders o {join} customers c USING(id)"),
            &["id", "customer_id", "amount", "name"],
            "sqlite",
        );
        complete(&graph);
        assert!(
            edge(&graph, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom)
                == (source != "s.customers.id")
        );
        assert!(
            edge(&graph, "s.v.id", "s.customers.id", EdgeKind::DerivesFrom)
                == (source != "s.orders.id")
        );
        assert!(edge(
            &graph,
            "s.v.customer_id",
            "s.orders.customer_id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(
            &graph,
            "s.v.name",
            "s.customers.name",
            EdgeKind::DerivesFrom
        ));
    }
}

#[test]
fn postgres_quoted_builtin_names_preserve_case() {
    let lower = scan(
        "SELECT upper(\"substring\"(name, 1, 1)) AS initial FROM customers",
        &["initial"],
    );
    complete(&lower);
    assert!(edge(
        &lower,
        "s.v.initial",
        "s.customers.name",
        EdgeKind::DerivesFrom
    ));

    let upper = scan(
        "SELECT \"SUBSTRING\"(name, 1, 1) AS initial FROM customers",
        &["initial"],
    );
    assert_eq!(
        upper.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&upper, "SG_CALL_UNRESOLVED"));
}

#[test]
fn grouping_having_window_and_ordering_are_analyzed() {
    let g=scan("SELECT customer_id AS c,SUM(amount) AS total FROM orders GROUP BY customer_id HAVING SUM(amount)>0 ORDER BY total",&["c","total"]);
    complete(&g);
    assert!(edge(
        &g,
        "s.v.total",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    let window = scan(
        "SELECT ROW_NUMBER() OVER(PARTITION BY customer_id ORDER BY amount) AS n FROM orders",
        &["n"],
    );
    complete(&window);
    assert!(edge(&window, "s.v", "s.orders.amount", EdgeKind::Reads));
    assert!(edge(
        &window,
        "s.v",
        "s.orders.customer_id",
        EdgeKind::Reads
    ));
}

#[test]
fn named_and_inherited_windows_match_inline_lineage() {
    let inline = scan(
        "SELECT ROW_NUMBER() OVER (PARTITION BY customer_id ORDER BY amount) AS position FROM orders",
        &["position"],
    );
    let named = scan(
        "SELECT ROW_NUMBER() OVER ranking AS position FROM orders WINDOW ranking AS (PARTITION BY customer_id ORDER BY amount)",
        &["position"],
    );
    let inherited = scan(
        "SELECT ROW_NUMBER() OVER ranking AS position FROM orders WINDOW base AS (PARTITION BY customer_id), ranking AS (base ORDER BY amount)",
        &["position"],
    );
    for graph in [&inline, &named, &inherited] {
        complete(graph);
        assert!(edge(
            graph,
            "s.v.position",
            "s.orders.customer_id",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(
            graph,
            "s.v.position",
            "s.orders.amount",
            EdgeKind::DerivesFrom
        ));
        assert!(edge(graph, "s.v", "s.orders.customer_id", EdgeKind::Reads));
        assert!(edge(graph, "s.v", "s.orders.amount", EdgeKind::Reads));
    }
}

#[test]
fn named_windows_are_query_local_and_unused_definitions_do_not_contaminate_values() {
    let unused = scan(
        "SELECT amount AS total FROM orders WINDOW unused AS (PARTITION BY customer_id ORDER BY id)",
        &["total"],
    );
    complete(&unused);
    assert!(edge(
        &unused,
        "s.v.total",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(edge(
        &unused,
        "s.v",
        "s.orders.customer_id",
        EdgeKind::Reads
    ));
    assert!(edge(&unused, "s.v", "s.orders.id", EdgeKind::Reads));

    let shadowed = scan(
        "SELECT (SELECT ROW_NUMBER() OVER ranking FROM orders inner_orders WINDOW ranking AS (PARTITION BY amount)) AS position FROM orders outer_orders WINDOW ranking AS (PARTITION BY customer_id)",
        &["position"],
    );
    complete(&shadowed);
    assert!(edge(
        &shadowed,
        "s.v.position",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &shadowed,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));

    let leaked = scan(
        "SELECT (SELECT ROW_NUMBER() OVER ranking FROM orders inner_orders) AS position FROM orders outer_orders WINDOW ranking AS (PARTITION BY customer_id)",
        &["position"],
    );
    assert_eq!(
        leaked.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&leaked, "SG_WINDOW_UNKNOWN"));
}

#[test]
fn invalid_named_windows_are_partial_without_guessing_sources() {
    let unknown = scan(
        "SELECT ROW_NUMBER() OVER missing AS position FROM orders",
        &["position"],
    );
    assert_eq!(
        unknown.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&unknown, "SG_WINDOW_UNKNOWN"));

    let cycle = scan(
        "SELECT ROW_NUMBER() OVER first_window AS position FROM orders WINDOW first_window AS (second_window), second_window AS (first_window)",
        &["position"],
    );
    assert_eq!(
        cycle.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&cycle, "SG_WINDOW_CYCLE"));

    let definitions = (0..=64)
        .map(|index| {
            if index == 64 {
                format!("w{index} AS (PARTITION BY id)")
            } else {
                format!("w{index} AS (w{})", index + 1)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let depth = scan(
        &format!("SELECT ROW_NUMBER() OVER w0 AS position FROM orders WINDOW {definitions}"),
        &["position"],
    );
    assert_eq!(
        depth.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&depth, "SG_WINDOW_DEPTH"));
}

#[test]
fn recursive_window_expressions_are_bounded_and_partial() {
    let graph = scan(
        "SELECT ROW_NUMBER() OVER recursive_window AS position FROM orders WINDOW recursive_window AS (PARTITION BY ROW_NUMBER() OVER recursive_window)",
        &["position"],
    );
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&graph, "SG_WINDOW_CYCLE"));
    assert!(!edge(
        &graph,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn branching_named_windows_have_a_shared_expansion_budget() {
    let mut definitions = vec!["w0 AS (PARTITION BY customer_id)".to_owned()];
    for index in 1..24 {
        let parent = index - 1;
        definitions.push(format!(
            "w{index} AS (PARTITION BY ROW_NUMBER() OVER w{parent} + ROW_NUMBER() OVER w{parent})"
        ));
    }
    let graph = scan(
        &format!(
            "SELECT ROW_NUMBER() OVER w23 AS position FROM orders WINDOW {}",
            definitions.join(", ")
        ),
        &["position"],
    );
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&graph, "SG_WINDOW_BUDGET"));
    assert!(!edge(
        &graph,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn named_window_diagnostics_taint_projection_lineage() {
    let duplicate = scan(
        "SELECT ROW_NUMBER() OVER ranking AS position FROM orders WINDOW ranking AS (PARTITION BY customer_id), ranking AS (PARTITION BY amount)",
        &["position"],
    );
    assert_eq!(
        duplicate.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(has_diagnostic(&duplicate, "SG_WINDOW_DUPLICATE"));
    assert!(!edge(
        &duplicate,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
    assert!(!edge(
        &duplicate,
        "s.v.position",
        "s.orders.amount",
        EdgeKind::DerivesFrom
    ));

    let unknown = scan(
        "SELECT ROW_NUMBER() OVER missing AS position FROM orders",
        &["position"],
    );
    let diagnostic = unknown.analysis()[&VertexId::from_raw("s.v")]
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "SG_WINDOW_UNKNOWN")
        .expect("unknown window diagnostic");
    assert!(diagnostic.location.is_some());
    assert!(!edge(
        &unknown,
        "s.v.position",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn order_by_output_alias_takes_precedence_over_input_column() {
    let g = scan("SELECT customer_id AS id FROM orders ORDER BY id", &["id"]);
    complete(&g);
    assert!(!edge(&g, "s.v", "s.orders.id", EdgeKind::Reads));
    assert!(edge(
        &g,
        "s.v.id",
        "s.orders.customer_id",
        EdgeKind::DerivesFrom
    ));
}

#[test]
fn constants_succeed_without_fabricating_a_relation() {
    let g = scan("SELECT 1 AS one", &["one"]);
    complete(&g);
    assert!(!g
        .outgoing(&VertexId::from_raw("s.v"))
        .iter()
        .any(|e| e.kind.is_dependency()));
}

#[test]
fn recursive_cte_is_partial_without_ghost_vertices() {
    let g=scan("WITH RECURSIVE q(id) AS (SELECT id FROM orders UNION ALL SELECT id FROM q) SELECT id FROM q",&["id"]);
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(edge(&g, "s.v", "s.orders.id", EdgeKind::Reads));
}

#[test]
fn missing_relation_blocks_wildcard_lineage_instead_of_shifting_columns() {
    let g = scan(
        "SELECT q.*,o.id FROM missing q CROSS JOIN orders o",
        &["id"],
    );
    assert_eq!(
        g.analysis()[&VertexId::from_raw("s.v")].state,
        AnalysisState::Partial
    );
    assert!(!edge(&g, "s.v.id", "s.orders.id", EdgeKind::DerivesFrom));
}

#[test]
fn sqlserver_database_qualifier_matches_the_collected_database_case_insensitively() {
    fn graph(relation: &str) -> Graph {
        let mut view = object("v", &["id", "amount"]);
        view.kind = "view".into();
        view.body = Some(format!("SELECT id, amount FROM {relation}"));
        let doc = CatalogDocument {
            context: Some(CollectionContext {
                source_id: "fixture".into(),
                database: Some("MyDb".into()),
                schema_filter: Some(vec!["dbo".into()]),
                catalog_complete: true,
                lower_case_table_names: None,
                collation: None,
            }),
            dependencies: Vec::new(),
            version: 1,
            dialect: "sqlserver".into(),
            reader: "fixture".into(),
            limitations: vec![],
            schemas: vec![SchemaDoc {
                name: "dbo".into(),
                objects: vec![
                    object("orders", &["id", "amount"]),
                    object("audit", &["id", "amount"]),
                    view,
                ],
                routines: vec![RoutineDoc {
                    name: "archive".into(),
                    kind: "procedure".into(),
                    language: Some("sql".into()),
                    body: Some(format!(
                        "INSERT INTO MyDb.dbo.audit(id, amount) SELECT id, amount FROM {relation}"
                    )),
                    signature: None,
                    usage: None,
                    member_of: None,
                    source: None,
                }],
            }],
        };
        let mut graph = schemagraph_source::graph::document_to_graph(&doc);
        let (_, notes) = super::enrich_from_document(&mut graph, &doc);
        for note in notes {
            graph.add_limitation(note);
        }
        graph
    }

    // SQL Server의 database 이름 비교는 수집된 database와 같은 규칙으로 접는다.
    for relation in ["MyDb.dbo.orders", "[MyDb].dbo.orders", "mydb.dbo.orders"] {
        let g = graph(relation);
        let context = format!("{relation}: {:?}", g.limitations());
        assert!(
            edge(&g, "dbo.v", "dbo.orders", EdgeKind::Reads),
            "{context}"
        );
        assert!(
            edge(&g, "dbo.v", "dbo.orders.id", EdgeKind::Reads),
            "{context}"
        );
        assert!(
            edge(
                &g,
                "dbo.v.amount",
                "dbo.orders.amount",
                EdgeKind::DerivesFrom
            ),
            "{context}"
        );
        assert!(
            edge(&g, "dbo.archive", "dbo.audit", EdgeKind::Writes),
            "{context}"
        );
        assert!(
            edge(&g, "dbo.archive", "dbo.orders", EdgeKind::Reads),
            "{context}"
        );
        assert!(
            edge(
                &g,
                "dbo.audit.amount",
                "dbo.orders.amount",
                EdgeKind::DerivesFrom
            ),
            "{context}"
        );
        assert!(!has_diagnostic(&g, "SG_CROSS_DATABASE"), "{context}");
        assert!(!has_diagnostic(&g, "SG_DML_SOURCE_SHAPE"), "{context}");
    }
    let other = graph("Warehouse.dbo.orders");
    assert!(!edge(&other, "dbo.v", "dbo.orders", EdgeKind::Reads));
    assert!(has_diagnostic(&other, "SG_CROSS_DATABASE"));
}

/// COPY FROM은 테이블 쓰기는 남기되, 외부 데이터가 채우는 컬럼별 효과는 알 수
/// 없어 `complete`로 주장하지 않는다. COPY TO는 읽기라 상태를 낮추지 않는다.
#[test]
fn copy_from_records_table_write_and_marks_column_effects_partial() {
    let graph = scan_routine_with(
        "BEGIN COPY orders (id, amount) FROM '/tmp/orders.csv'; COPY customers TO STDOUT; END",
        "postgres",
        "plpgsql",
    );
    assert!(edge(&graph, "s.mutate", "s.orders", EdgeKind::Writes));
    assert!(edge(&graph, "s.mutate", "s.customers", EdgeKind::Reads));
    assert!(has_diagnostic(&graph, "SG_DML_COPY"));
    assert_ne!(
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Complete
    );
}

#[test]
fn copy_to_alone_keeps_routine_complete() {
    let graph = scan_routine_with(
        "BEGIN COPY (SELECT id FROM orders) TO STDOUT; END",
        "postgres",
        "plpgsql",
    );
    assert!(edge(&graph, "s.mutate", "s.orders", EdgeKind::Reads));
    assert!(!has_diagnostic(&graph, "SG_DML_COPY"));
    assert_eq!(
        graph.analysis()[&VertexId::from_raw("s.mutate")].state,
        AnalysisState::Complete,
        "{:?}",
        graph.limitations()
    );
}

/// COPY의 컬럼 목록은 SELECT·INSERT의 컬럼 목록과 같은 근거다. 그 컬럼을 지우면
/// COPY가 깨지므로 impact가 컬럼에서 이 routine에 닿아야 한다. 목록이 없으면
/// `SELECT *`처럼 알려진 컬럼 전체를 쓴다.
#[test]
fn copy_column_lists_record_column_reads_and_writes() {
    let graph = scan_routine_with(
        "BEGIN COPY orders (amount) TO '/tmp/x.csv'; COPY audit (name) FROM '/tmp/a.csv'; COPY customers TO STDOUT; END",
        "postgres",
        "plpgsql",
    );
    assert!(edge(&graph, "s.mutate", "s.orders.amount", EdgeKind::Reads));
    assert!(!edge(&graph, "s.mutate", "s.orders.id", EdgeKind::Reads));
    assert!(edge(&graph, "s.mutate", "s.audit.name", EdgeKind::Writes));
    assert!(!edge(&graph, "s.mutate", "s.audit.id", EdgeKind::Writes));
    assert!(edge(&graph, "s.mutate", "s.customers.id", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.mutate",
        "s.customers.name",
        EdgeKind::Reads
    ));
}

/// `--sql-dir`의 SQL 파일은 sql 언어 routine으로 붙는다. pg_dump 형식의
/// `COPY … FROM STDIN;` 데이터 블록도 적재 대상 쓰기로 남는다.
#[test]
fn sql_file_copy_from_stdin_records_table_write() {
    for body in [
        "COPY orders (id, amount) FROM STDIN;\n1\t10\n\\.\n",
        "COPY orders (id, amount) FROM STDIN;",
    ] {
        let graph = scan_routine_with(body, "postgres", "sql");
        assert!(
            edge(&graph, "s.mutate", "s.orders", EdgeKind::Writes),
            "{body:?}: {:?}",
            graph.limitations()
        );
        assert!(has_diagnostic(&graph, "SG_DML_COPY"), "{body:?}");
    }
}

/// 대소문자 규칙 회귀를 위한 문서다. `routines`의 첫 항목이 호출자다.
fn case_doc(dialect: &str, trigger_body: Option<&str>, routines: Vec<RoutineDoc>) -> Graph {
    let mut orders = object("orders", &["id", "customer_id", "amount"]);
    if let Some(body) = trigger_body {
        orders.triggers.push(TriggerDoc {
            name: "trg".into(),
            body: Some(body.into()),
        });
    }
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            objects: vec![orders, object("audit", &["id", "name"])],
            routines,
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

fn case_routine(name: &str, kind: &str, body: &str) -> RoutineDoc {
    RoutineDoc {
        name: name.into(),
        kind: kind.into(),
        language: Some("sql".into()),
        body: Some(body.into()),
        signature: None,
        usage: None,
        member_of: None,
        source: None,
    }
}

/// SQLite 식별자는 ASCII 대소문자를 구분하지 않는다. 트리거 몸체의 `AUDIT`도
/// 카탈로그의 audit 테이블 쓰기다.
#[test]
fn sqlite_body_relations_fold_ascii_case() {
    let graph = case_doc(
        "sqlite",
        Some("CREATE TRIGGER trg AFTER INSERT ON orders BEGIN INSERT INTO AUDIT(id, name) VALUES (NEW.id, 'x'); END"),
        vec![],
    );
    let trigger = graph
        .vertices()
        .find(|vertex| vertex.kind == schemagraph_core::VertexKind::Trigger)
        .expect("trigger vertex")
        .id
        .as_str()
        .to_owned();
    assert!(
        edge(&graph, &trigger, "s.audit", EdgeKind::Writes),
        "{:?}",
        graph.limitations()
    );
    assert!(
        !graph.limitations().iter().any(|l| l.contains("AUDIT")),
        "{:?}",
        graph.limitations()
    );
}

/// SQLite 대소문자 규칙 회귀용 문서다. 스키마 `s` 하나에 객체와 routine을 담는다.
fn sqlite_doc(objects: Vec<ObjectDoc>, routines: Vec<RoutineDoc>) -> Graph {
    dialect_doc("sqlite", objects, routines)
}

/// `sqlite_doc`과 같은 모양을 다른 방언으로 만든다. SQLite 전용 규칙이 다른 방언으로
/// 번지지 않는지 같은 입력으로 비교하는 데 쓴다.
fn dialect_doc(dialect: &str, objects: Vec<ObjectDoc>, routines: Vec<RoutineDoc>) -> Graph {
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            objects,
            routines,
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    for edge in graph.edges() {
        assert!(graph.vertex(&edge.from).is_some());
        assert!(graph.vertex(&edge.to).is_some());
    }
    graph
}

fn sqlite_view(name: &str, outputs: &[&str], body: &str) -> ObjectDoc {
    let mut view = object(name, outputs);
    view.kind = "view".into();
    view.body = Some(body.into());
    view
}

/// `from`에서 `target` 객체나 그 컬럼으로 가는 간선이 하나라도 있는가.
fn touches(g: &Graph, from: &str, target: &str) -> bool {
    let member = format!("{target}.");
    g.edges().iter().any(|e| {
        e.from.as_str() == from && (e.to.as_str() == target || e.to.as_str().starts_with(&member))
    })
}

fn trigger_id(g: &Graph) -> String {
    g.vertices()
        .find(|vertex| vertex.kind == schemagraph_core::VertexKind::Trigger)
        .expect("trigger vertex")
        .id
        .as_str()
        .to_owned()
}

/// SQLite는 식별자의 ASCII 대소문자만 무시한다(`sqlite3_stricmp`의 정의,
/// https://www.sqlite.org/c3ref/stricmp.html). 실제 SQLite에서 `SELECT * FROM étudiants`는
/// `ÉTUDIANTS`만 있으면 `no such table`이고, ASCII 글자만 다른 `Étudiants`는 같은 테이블이다.
/// Unicode 소문자로 접으면 앞의 몸체에 거짓 reads 간선이 생긴다.
#[test]
fn sqlite_relation_names_fold_only_ascii_case() {
    let graph = sqlite_doc(
        vec![
            object("ÉTUDIANTS", &["id"]),
            sqlite_view("v_unicode", &["id"], "SELECT id FROM étudiants"),
            sqlite_view("v_ascii", &["id"], "SELECT ID FROM Étudiants"),
        ],
        vec![],
    );
    assert!(
        !touches(&graph, "s.v_unicode", "s.ÉTUDIANTS"),
        "{:?}",
        graph.edges()
    );
    assert!(
        graph
            .limitations()
            .iter()
            .any(|l| l.contains("relation 's.étudiants' is missing")),
        "{:?}",
        graph.limitations()
    );
    assert!(edge(&graph, "s.v_ascii", "s.ÉTUDIANTS", EdgeKind::Reads));
    assert!(edge(&graph, "s.v_ascii", "s.ÉTUDIANTS.id", EdgeKind::Reads));
    assert!(edge(
        &graph,
        "s.v_ascii.id",
        "s.ÉTUDIANTS.id",
        EdgeKind::DerivesFrom
    ));
}

/// 켈빈 기호(U+212A)는 Unicode 소문자 규칙으로 `k`가 되지만 SQLite에서는 다른 이름이다.
/// 실제 SQLite도 `"K"` 테이블에 `SELECT * FROM k`를 `no such table`로 거절한다.
#[test]
fn sqlite_kelvin_sign_is_not_the_letter_k() {
    let kelvin = "\u{212A}";
    let graph = sqlite_doc(
        vec![
            object(kelvin, &["id"]),
            sqlite_view("v", &["id"], "SELECT id FROM k"),
        ],
        vec![],
    );
    assert!(
        !touches(&graph, "s.v", &format!("s.{kelvin}")),
        "{:?}",
        graph.edges()
    );
    let graph = sqlite_doc(
        vec![
            object("k", &["id"]),
            sqlite_view("v", &["id"], &format!("SELECT id FROM \"{kelvin}\"")),
        ],
        vec![],
    );
    assert!(!touches(&graph, "s.v", "s.k"), "{:?}", graph.edges());
    // ASCII `K`는 같은 테이블이다.
    let graph = sqlite_doc(
        vec![
            object("k", &["id"]),
            sqlite_view("v", &["id"], "SELECT ID FROM K"),
        ],
        vec![],
    );
    assert!(edge(&graph, "s.v", "s.k", EdgeKind::Reads));
    assert!(edge(&graph, "s.v", "s.k.id", EdgeKind::Reads));
}

/// SQLite 스키마에는 `ÉTUDIANTS`와 `étudiants`가 함께 있을 수 있고, 몸체의 `étudiants`는
/// 뒤의 테이블만 읽는다. Unicode로 접으면 두 후보가 되어 해석을 포기했다.
#[test]
fn sqlite_unicode_case_variants_resolve_to_their_own_table() {
    let graph = sqlite_doc(
        vec![
            object("ÉTUDIANTS", &["id"]),
            object("étudiants", &["id"]),
            sqlite_view("v", &["id"], "SELECT id FROM étudiants"),
        ],
        vec![],
    );
    assert!(
        edge(&graph, "s.v", "s.étudiants", EdgeKind::Reads),
        "{:?}",
        graph.limitations()
    );
    assert!(edge(&graph, "s.v", "s.étudiants.id", EdgeKind::Reads));
    assert!(
        !touches(&graph, "s.v", "s.ÉTUDIANTS"),
        "{:?}",
        graph.edges()
    );
}

/// 트리거의 객체 단위 쓰기(`vertex_hit`의 ASCII 접기)와 컬럼 단위 쓰기(Binder)는 같은
/// 규칙을 따라야 한다. 컬럼 단위만 Unicode로 접어 `étudiants_log`가 `ÉTUDIANTS_LOG`
/// 테이블 쓰기는 "없음"인데 그 컬럼 쓰기는 남았다.
#[test]
fn sqlite_trigger_object_and_column_writes_agree_on_case() {
    for (target, resolves) in [("étudiants_log(n)", false), ("Étudiants_LOG(N)", true)] {
        let mut orders = object("orders", &["id"]);
        orders.triggers.push(TriggerDoc {
            name: "trg".into(),
            body: Some(format!(
                "CREATE TRIGGER trg AFTER INSERT ON orders BEGIN INSERT INTO {target} VALUES (NEW.id); END"
            )),
        });
        let graph = sqlite_doc(vec![orders, object("ÉTUDIANTS_LOG", &["n"])], vec![]);
        let trigger = trigger_id(&graph);
        let detail = format!("{target}: {:?} {:?}", graph.edges(), graph.limitations());
        assert_eq!(
            edge(&graph, &trigger, "s.ÉTUDIANTS_LOG", EdgeKind::Writes),
            resolves,
            "{detail}"
        );
        assert_eq!(
            edge(&graph, &trigger, "s.ÉTUDIANTS_LOG.n", EdgeKind::Writes),
            resolves,
            "{detail}"
        );
        assert_eq!(
            touches(&graph, &trigger, "s.ÉTUDIANTS_LOG"),
            resolves,
            "{detail}"
        );
        if !resolves {
            assert!(
                graph
                    .limitations()
                    .iter()
                    .any(|l| l.contains("references s.étudiants_log, which is missing")),
                "{detail}"
            );
        }
    }
}

/// SQLite 컬럼 이름도 ASCII 대소문자만 무시한다. 실제 SQLite는 `Prénom` 컬럼에
/// `SELECT PRÉNOM`을 `no such column`으로 거절하고 `SELECT PRéNOM`은 받는다.
#[test]
fn sqlite_column_names_fold_only_ascii_case() {
    let graph = sqlite_doc(
        vec![
            object("personnes", &["Prénom"]),
            sqlite_view("v_unicode", &["p"], "SELECT PRÉNOM AS p FROM personnes"),
            sqlite_view("v_ascii", &["p"], "SELECT PRéNOM AS p FROM personnes"),
        ],
        vec![],
    );
    assert!(
        !touches(&graph, "s.v_unicode", "s.personnes.Prénom"),
        "{:?}",
        graph.edges()
    );
    assert!(
        !edge(
            &graph,
            "s.v_unicode.p",
            "s.personnes.Prénom",
            EdgeKind::DerivesFrom
        ),
        "{:?}",
        graph.edges()
    );
    assert!(
        graph
            .limitations()
            .iter()
            .any(|l| l.starts_with("view s.v_unicode")
                && l.contains("SG_COLUMN_UNRESOLVED")
                && l.contains("column 'prÉnom'")),
        "{:?}",
        graph.limitations()
    );
    assert!(edge(
        &graph,
        "s.v_ascii",
        "s.personnes.Prénom",
        EdgeKind::Reads
    ));
    assert!(edge(
        &graph,
        "s.v_ascii.p",
        "s.personnes.Prénom",
        EdgeKind::DerivesFrom
    ));
}

/// SQLite 몸체의 routine 호출도 카탈로그 routine 키와 같은 ASCII 규칙으로 비교한다.
/// `écho`와 `Écho`는 SQLite에서 다른 이름이라 함께 있을 수 있다. `ÉCHO(...)`는 ASCII만
/// 접으면 `Écho`이고, `éCHO(...)`는 `écho`다. Unicode로 접으면 둘 다 `écho`로 갔다.
#[test]
fn sqlite_routine_names_fold_only_ascii_case() {
    let graph = sqlite_doc(
        vec![
            object("t", &["id"]),
            sqlite_view("v_upper", &["x"], "SELECT ÉCHO(id) AS x FROM t"),
            sqlite_view("v_lower", &["x"], "SELECT éCHO(id) AS x FROM t"),
        ],
        vec![
            case_routine("écho", "function", "SELECT 1"),
            case_routine("Écho", "function", "SELECT 1"),
        ],
    );
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert!(
        edge(&graph, "s.v_upper", "s.Écho", EdgeKind::Calls),
        "{detail}"
    );
    assert!(
        !edge(&graph, "s.v_upper", "s.écho", EdgeKind::Calls),
        "{detail}"
    );
    assert!(
        edge(&graph, "s.v_lower", "s.écho", EdgeKind::Calls),
        "{detail}"
    );
    assert!(
        !edge(&graph, "s.v_lower", "s.Écho", EdgeKind::Calls),
        "{detail}"
    );
}

/// SQLite 테이블·뷰는 컬럼이 0개일 수 없다. 컬럼 없는 SQLite 객체는 수집기가 컬럼을 읽지
/// 못한 것(SQLite가 컴파일하지 못한 뷰)이다. 그 위의 `*`가 아무것도 펼치지 않는다고 확정하면
/// 깨진 뷰를 읽는 몸체가 진단 없이 `complete`가 된다.
#[test]
fn sqlite_object_without_columns_is_an_unknown_relation() {
    let graph = sqlite_doc(
        vec![
            sqlite_view("v_broken", &[], "SELECT id FROM gone"),
            sqlite_view("v_on_broken", &[], "SELECT * FROM v_broken"),
        ],
        vec![],
    );
    let analysis = &graph.analysis()[&VertexId::from_raw("s.v_on_broken")];
    let detail = format!("{:?} {:?}", analysis.diagnostics, graph.limitations());
    assert_eq!(analysis.state, AnalysisState::Partial, "{detail}");
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "SG_WILDCARD_UNKNOWN"),
        "{detail}"
    );
    assert!(edge(&graph, "s.v_on_broken", "s.v_broken", EdgeKind::Reads));
}

/// PostgreSQL은 컬럼 없는 테이블(`CREATE TABLE t()`)을 허용한다. 그 `*`는 실제로 아무것도
/// 펼치지 않으므로, SQLite의 "컬럼 없음 = 모름" 규칙이 다른 방언으로 번지면 안 된다.
#[test]
fn postgres_table_without_columns_stays_a_known_relation() {
    let graph = dialect_doc(
        "postgres",
        vec![
            object("empty", &[]),
            sqlite_view("v", &[], "SELECT * FROM empty"),
        ],
        vec![],
    );
    complete(&graph);
    assert!(edge(&graph, "s.v", "s.empty", EdgeKind::Reads));
}

/// SQLite 트리거의 NEW/OLD는 소유 테이블이다. 그 이름에 대문자가 있으면(`"Pedidos"`, EF Core의
/// `Blogs`) Binder 색인과 같은 `catalog_key`로 찾아야 한다. 원래 철자로 찾으면 거짓
/// `relation 's.Pedidos' is missing`·`transition column 'new.qty'` 진단이 나고, 객체 단위
/// `reads`는 있는데 `derives-from` 간선이 빠졌다.
#[test]
fn sqlite_trigger_transition_table_keeps_its_letter_case() {
    let mut pedidos = object("Pedidos", &["qty"]);
    pedidos.triggers.push(TriggerDoc {
        name: "trg_p".into(),
        body: Some(
            "CREATE TRIGGER trg_p AFTER INSERT ON \"Pedidos\" BEGIN INSERT INTO log(n) VALUES (NEW.qty); END"
                .into(),
        ),
    });
    let graph = sqlite_doc(vec![pedidos, object("log", &["n"])], vec![]);
    let trigger = trigger_id(&graph);
    let analysis = &graph.analysis()[&VertexId::from_raw(&trigger)];
    let detail = format!("{:?} {:?}", graph.edges(), graph.limitations());
    assert_eq!(analysis.state, AnalysisState::Complete, "{detail}");
    assert!(analysis.diagnostics.is_empty(), "{detail}");
    assert!(
        edge(&graph, "s.log.n", "s.Pedidos.qty", EdgeKind::DerivesFrom),
        "{detail}"
    );
    assert!(
        edge(&graph, &trigger, "s.log.n", EdgeKind::Writes),
        "{detail}"
    );
}

/// SQLite CTE 이름도 ASCII 대소문자만 무시한다. `WITH Recent AS (…) … FROM recent`의 `recent`는
/// 같은 이름의 실제 테이블이 있어도 CTE다(실제 SQLite도 CTE 값을 넣는다). 트리거와 DML 쿼리
/// 파일의 객체 단위 수집이 CTE 이름을 정확히 비교하면 실제 테이블로 가는 거짓 `reads`가 났다.
#[test]
fn sqlite_dml_cte_names_fold_ascii_case() {
    let body =
        "INSERT INTO log(n) SELECT id FROM (WITH Recent AS (SELECT 7 AS id) SELECT id FROM recent)";
    let mut orders = object("orders", &["id"]);
    orders.triggers.push(TriggerDoc {
        name: "t_cte".into(),
        body: Some(format!(
            "CREATE TRIGGER t_cte AFTER INSERT ON orders BEGIN {body}; END"
        )),
    });
    let mut query = case_routine("dml_cte", "query", body);
    query.language = None;
    query.source = Some("dml/cte.sql".into());
    let graph = sqlite_doc(
        vec![orders, object("log", &["n"]), object("recent", &["id"])],
        vec![query],
    );
    let trigger = trigger_id(&graph);
    for owner in [trigger.as_str(), "s.dml_cte"] {
        let detail = format!("{owner}: {:?} {:?}", graph.edges(), graph.limitations());
        assert!(edge(&graph, owner, "s.log", EdgeKind::Writes), "{detail}");
        assert!(!touches(&graph, owner, "s.recent"), "{detail}");
    }
}

/// MySQL routine 이름은 플랫폼과 무관하게 대소문자를 구분하지 않는다. 테이블
/// 이름은 lower_case_table_names에 달려 있어 기본으로 접지 않는다.
#[test]
fn mysql_routine_calls_fold_case_but_tables_do_not_by_default() {
    let graph = case_doc(
        "mysql",
        None,
        vec![
            case_routine(
                "caller",
                "procedure",
                "BEGIN CALL P_Helper(); SELECT FN_TOTAL(id) FROM ORDERS; END",
            ),
            case_routine("p_helper", "procedure", "BEGIN SELECT 1; END"),
            case_routine("fn_total", "function", "RETURN 1"),
        ],
    );
    assert!(
        edge(&graph, "s.caller", "s.p_helper", EdgeKind::Calls),
        "{:?}",
        graph.limitations()
    );
    assert!(
        edge(&graph, "s.caller", "s.fn_total", EdgeKind::Calls),
        "{:?}",
        graph.limitations()
    );
    // ORDERS ≠ orders: lower_case_table_names=0(리눅스 기본)이면 다른 테이블일 수 있다.
    assert!(!edge(&graph, "s.caller", "s.orders", EdgeKind::Reads));
}

/// routine 이름의 대소문자 무시는 이름에만 적용된다. DB(스키마) 한정자는 테이블
/// 이름과 같은 lower_case_table_names 규칙을 따르므로, 규칙을 모르면 정확히 비교한다.
#[test]
fn mysql_routine_folding_keeps_database_qualifiers_exact() {
    let graph = case_doc(
        "mysql",
        None,
        vec![
            case_routine("caller", "procedure", "BEGIN CALL S.p_helper(); END"),
            case_routine("other", "procedure", "BEGIN CALL s.P_HELPER(); END"),
            case_routine("p_helper", "procedure", "BEGIN SELECT 1; END"),
        ],
    );
    assert!(!edge(&graph, "s.caller", "s.p_helper", EdgeKind::Calls));
    assert!(
        edge(&graph, "s.other", "s.p_helper", EdgeKind::Calls),
        "{:?}",
        graph.limitations()
    );
}

/// 대소문자 규칙을 실은 문맥으로 MySQL·SQL Server 문서를 만든다.
fn case_rule_doc(
    dialect: &str,
    lower_case_table_names: Option<u8>,
    collation: Option<&str>,
    body: &str,
) -> Graph {
    let doc = CatalogDocument {
        context: Some(CollectionContext {
            source_id: "app".into(),
            database: None,
            schema_filter: None,
            catalog_complete: true,
            lower_case_table_names,
            collation: collation.map(str::to_owned),
        }),
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            objects: vec![object("Orders", &["id"])],
            routines: vec![case_routine("reader", "procedure", body)],
        }],
    };
    let mut graph = schemagraph_source::graph::document_to_graph(&doc);
    let (_, notes) = super::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    graph
}

/// 서버가 알려 준 대소문자 규칙이 있을 때만 관계 이름을 접는다.
#[test]
fn mysql_and_sqlserver_fold_relation_case_only_with_collected_rules() {
    let mysql = "BEGIN SELECT id FROM orders; END";
    let sqlserver = "SELECT id FROM orders";
    let reads = |graph: &Graph| edge(graph, "s.reader", "s.Orders", EdgeKind::Reads);
    assert!(!reads(&case_rule_doc("mysql", None, None, mysql)));
    assert!(!reads(&case_rule_doc("mysql", Some(0), None, mysql)));
    assert!(reads(&case_rule_doc("mysql", Some(1), None, mysql)));
    assert!(reads(&case_rule_doc("mariadb", Some(2), None, mysql)));
    assert!(!reads(&case_rule_doc("sqlserver", None, None, sqlserver)));
    assert!(!reads(&case_rule_doc(
        "sqlserver",
        None,
        Some("Latin1_General_CS_AS"),
        sqlserver
    )));
    assert!(!reads(&case_rule_doc(
        "sqlserver",
        None,
        Some("Latin1_General_100_BIN2"),
        sqlserver
    )));
    assert!(reads(&case_rule_doc(
        "sqlserver",
        None,
        Some("SQL_Latin1_General_CP1_CI_AS"),
        sqlserver
    )));
    // 터키어·아제르바이잔어 CI는 i와 I를 같은 글자로 보지 않는다(ı·İ). ASCII 접기로는
    // 그 규칙을 낼 수 없으므로 접지 않는다 — `FROM INVOICES`가 invoices에 이어지면 안 된다.
    for collation in [
        "Turkish_CI_AS",
        "Turkish_100_CI_AS",
        "Azeri_Latin_100_CI_AS",
        "SQL_Latin1_General_CP1254_CI_AS",
    ] {
        assert!(
            !reads(&case_rule_doc(
                "sqlserver",
                None,
                Some(collation),
                sqlserver
            )),
            "{collation}"
        );
    }
}
