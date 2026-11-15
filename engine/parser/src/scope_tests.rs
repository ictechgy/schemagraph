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
    let doc = CatalogDocument {
        context: None,
        dependencies: Vec::new(),
        version: 1,
        dialect: dialect.into(),
        reader: "fixture".into(),
        limitations: vec![],
        schemas: vec![SchemaDoc {
            name: "s".into(),
            objects: vec![
                object("orders", &["id", "customer_id", "amount"]),
                object("customers", &["id", "name"]),
                object("audit", &["id", "name"]),
            ],
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
