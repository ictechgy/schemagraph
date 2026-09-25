//! schemagraph CLI — scan으로 그래프를 만들고, 질의는 graph.json 위에서 한다.
//!
//! 종료 코드 계약:
//! - 0: 성공 (또는 보고할 것이 없음)
//! - 1: 질의가 발견을 보고함(--strict 시) 또는 대상을 못 찾음(notFound)
//! - 2: 사용법·엔진 오류 (잘못된 URL, 파일 없음, 파싱 불가)
//! - 130: Ctrl+C로 질의·경로·검토 작업을 취소함

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use schemagraph_analysis::{self as analysis, Resolve};
use schemagraph_core::{Graph, Level};
use schemagraph_export::{self as export, GraphDoc};
use schemagraph_source::{self as source};
use serde::Deserialize;
use std::io::{BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

mod cache;
mod cancellation;
mod import;
mod mcp;
mod merge;
mod policy;
mod review;

#[derive(Parser)]
#[command(
    name = "schemagraph",
    version,
    about = "Build a database dependency graph and inspect dependencies, evidence, and changes"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Attach offline dbt compiled SQL or observed queries to a catalog document.
    Import {
        catalog: PathBuf,
        #[arg(long, value_enum)]
        format: ImportFormat,
        #[arg(long)]
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        report: PathBuf,
        /// Root directory for validating dbt source and compiled SQL paths.
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, default_value_t = 67_108_864, value_parser = clap::value_parser!(u64).range(1..=536_870_912))]
        max_input_bytes: u64,
        #[arg(long, default_value_t = 100_000, value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
        max_rows: u32,
        #[arg(long, default_value_t = 8_388_608, value_parser = clap::value_parser!(u64).range(1..=67_108_864))]
        max_sql_bytes: u64,
        #[arg(long, default_value_t = 67_108_864, value_parser = clap::value_parser!(u64).range(1..=536_870_912))]
        max_total_sql_bytes: u64,
    },
    /// Read a database catalog and write the dependency graph (the artifact).
    Scan {
        /// Connection URL: sqlite:PATH, postgres://…, mysql://… — omit with --document.
        url: Option<String>,
        /// Output path for graph.json ('-' for stdout).
        #[arg(short, long, default_value = "graph.json")]
        output: String,
        /// Also dump the raw catalog document to this path (debugging / fixtures).
        #[arg(long)]
        emit_document: Option<PathBuf>,
        /// Wire version for --emit-document; v1 remains the compatibility default.
        #[arg(long, requires = "emit_document", value_parser = clap::value_parser!(u32).range(1..=2))]
        document_version: Option<u32>,
        /// Build the graph from a catalog document (probe output) instead of a live URL.
        #[arg(long)]
        document: Option<PathBuf>,
        /// Add name-heuristic `inferred` edges (opt-in — never a dependency basis).
        #[arg(long)]
        inferred: bool,
        /// Logical source label for comparing snapshots; never a connection URL.
        #[arg(long)]
        source_id: Option<String>,
        /// Restrict the collected document to these schemas.
        #[arg(long, value_delimiter = ',')]
        schema: Vec<String>,
        /// Also collect native DB dependency catalog facts when supported.
        #[arg(long)]
        catalog_dependencies: bool,
        /// Add declared application SQL uses from a directory of .sql files.
        #[arg(long)]
        sql_dir: Option<PathBuf>,
        /// Default schema for application SQL (required when several are collected).
        #[arg(long, requires = "sql_dir")]
        query_schema: Option<String>,
        /// Reuse SQL body analysis from this local directory (catalog and usage stay fresh).
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Render the graph as Mermaid, JSON, DOT, or offline HTML.
    Graph {
        /// Input graph.json produced by `scan`.
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, value_enum, default_value_t = GraphFormat::Json)]
        format: GraphFormat,
        /// Aggregation level (column = member level).
        #[arg(long, value_enum)]
        level: Option<LevelArg>,
    },
    /// Find vertices by name substring or `*`/`?` glob (case-insensitive).
    Search {
        /// Substring of an id or name, or a glob over the whole id.
        pattern: String,
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Restrict matches to one vertex kind (for example table, view, column).
        #[arg(long)]
        kind: Option<String>,
        /// `names` lists ids; `summary` adds kind, schema, name, and neighbor counts.
        #[arg(long, value_enum, default_value_t = DetailArg::Names)]
        detail: DetailArg,
        /// Max matches before `truncated` is reported.
        #[arg(long, default_value_t = 100)]
        max: usize,
    },
    /// Who depends on this object / what does it depend on (agent JSON).
    Query {
        /// Object name or qualified id (schema.object[.member]).
        name: String,
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, default_value_t = 1)]
        depth: u32,
        /// Max neighbors per direction before `truncated` is reported.
        #[arg(long, default_value_t = 256)]
        max: usize,
        /// Max vertices visited by each directional traversal.
        #[arg(long, default_value_t = 100_000)]
        max_visited: usize,
        /// Max dependency edges examined by each directional traversal.
        #[arg(long, default_value_t = 1_000_000)]
        max_examined_edges: usize,
    },
    /// What breaks if this object is changed or dropped (reverse transitive closure).
    Impact {
        /// Object name or qualified id (schema.object[.member]).
        name: String,
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Max impacted objects before `truncated` is reported.
        #[arg(long, default_value_t = 1024)]
        max: usize,
        /// Max vertices visited by the reverse traversal.
        #[arg(long, default_value_t = 100_000)]
        max_visited: usize,
        /// Max dependency edges examined by the reverse traversal.
        #[arg(long, default_value_t = 1_000_000)]
        max_examined_edges: usize,
    },
    /// Dead-object candidates: consumers nothing in the database references.
    Dead {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Max candidates before `truncated` is reported.
        #[arg(long, default_value_t = 1024)]
        max: usize,
        /// Exit 1 when any candidate is reported (for CI).
        #[arg(long)]
        strict: bool,
        /// Optional TOML policy; schemagraph.toml is loaded when present.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Preserve matching entry points and their transitive dependencies.
        #[arg(long, value_delimiter = ',')]
        retain: Vec<String>,
        /// Reproducible date used to evaluate expiring suppressions.
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Report tables and indexes with no observed reads in their statistics window.
    Unused {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Max candidates before `truncated` is reported.
        #[arg(long, default_value_t = 1024)]
        max: usize,
        /// Exit 1 when any candidate is reported (for CI).
        #[arg(long)]
        strict: bool,
    },
    /// Report structured per-object analysis coverage and diagnostic codes.
    Diagnostics {
        name: Option<String>,
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
    },
    /// Explain incident edges using their catalog or SQL provenance.
    Explain {
        name: String,
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, default_value_t = 1024)]
        max: usize,
    },
    /// Find shortest dependency paths (use --reverse for impact direction).
    Path {
        from: String,
        to: String,
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, default_value_t = 32)]
        max_paths: usize,
        #[arg(long, default_value_t = 32)]
        depth: u32,
        #[arg(long, default_value_t = 100_000)]
        max_visited: usize,
        #[arg(long, default_value_t = 1_000_000)]
        max_edges: usize,
        #[arg(long)]
        reverse: bool,
    },
    /// Report dependency cycles (delete ordering / deadlock analysis).
    Cycles {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, value_enum, default_value_t = LevelArg::Object)]
        level: LevelArg,
        /// Exit 1 when cycles are found (CI gate).
        #[arg(long)]
        strict: bool,
    },
    /// Report collected usage statistics (evidence, not a deletion verdict).
    Stats {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
    },
    /// Compare two snapshots — graph.json↔graph.json or document↔document.
    Diff {
        /// Older snapshot (graph.json or catalog document).
        old: PathBuf,
        /// Newer snapshot (same artifact kind as old).
        new: PathBuf,
        /// Exit 1 when any difference is found (CI drift gate).
        #[arg(long)]
        strict: bool,
    },
    /// Review catalog changes and trace their dependents in the previous snapshot.
    Review {
        before: PathBuf,
        after: PathBuf,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        require_complete: bool,
        /// Apply a versioned review policy without changing graph facts.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Mark previously reviewed finding fingerprints as existing.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Evaluate expiring waivers at this explicit YYYY-MM-DD date.
        #[arg(long)]
        as_of: Option<String>,
        /// Write a baseline only for a complete, comparable review.
        #[arg(long)]
        write_baseline: Option<PathBuf>,
        #[arg(long, default_value_t = 256)]
        max_changes: usize,
        #[arg(long, default_value_t = 1024)]
        max_impacted: usize,
        /// Maximum vertices per changed object's impact traversal.
        #[arg(long, default_value_t = 100_000)]
        max_visited: usize,
        /// Maximum examined edges per changed object's impact traversal.
        #[arg(long, default_value_t = 1_000_000)]
        max_examined_edges: usize,
        #[arg(long,value_enum,default_value_t=ReviewFormat::Json)]
        format: ReviewFormat,
    },
    /// Serve read-only MCP tools over one preloaded graph (stdio JSON-RPC).
    Serve {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Retention policy for the `dead` tool; schemagraph.toml is not read implicitly.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Retention roots for the `dead` tool, as with `dead --retain`.
        #[arg(long, value_delimiter = ',')]
        retain: Vec<String>,
        /// Date for expiring suppressions, as with `dead --as-of`.
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Merge catalog documents under their explicit source-id namespaces.
    Merge {
        #[arg(required = true, num_args = 2..)]
        documents: Vec<PathBuf>,
        #[arg(short, long, default_value = "graph.json")]
        output: String,
    },
    /// Export column lineage as OpenLineage RunEvents (NDJSON, one event per SQL body).
    Openlineage {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Dataset and job namespace, for example postgres://db.example:5432.
        #[arg(long)]
        namespace: String,
        /// Database name prefixed to dataset names (database.schema.object).
        #[arg(long)]
        database: Option<String>,
        /// RFC 3339 event time; defaults to now. Fix it for reproducible output.
        #[arg(long)]
        event_time: Option<String>,
        /// Output path; '-' or omitted writes to stdout.
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Export catalog declarations as isthmus bridge-facts (persistence target).
    Facts {
        /// Catalog document to convert (probe output or scan --emit-document).
        #[arg(long)]
        document: PathBuf,
        /// Project root shared with code-side producer documents (the join key).
        #[arg(long, default_value = ".")]
        project: PathBuf,
        /// Output path; '-' or omitted writes to stdout.
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Report schema facts and unresolved SQL references from the graph.
    Lint {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, default_value_t = 256)]
        max: usize,
        /// Exit 1 for confirmed non-advisory findings, or 2 when coverage is incomplete.
        #[arg(long)]
        strict: bool,
    },
    /// Print the agent skill document for consuming this tool's output.
    Skill,
    /// Describe catalog versions and features so producers can choose a compatible format.
    DocumentCapabilities,
    /// Check declared dependency rules against the graph (CI gate).
    Rules {
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        /// Rules file (TOML). Each [[rule]] declares name/from/to(/kinds).
        #[arg(short, long, default_value = "schemagraph.toml")]
        config: PathBuf,
        /// Exit 1 when any rule is violated.
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum GraphFormat {
    Mermaid,
    Json,
    Dot,
    Html,
}

/// `search --detail`의 CLI 값 — 와이어 라벨은 export가 소유한다.
#[derive(Clone, Copy, ValueEnum)]
enum DetailArg {
    Names,
    Summary,
}

impl DetailArg {
    /// export의 공개 단계로 바꾼다.
    fn detail(self) -> export::search::SearchDetail {
        match self {
            Self::Names => export::search::SearchDetail::Names,
            Self::Summary => export::search::SearchDetail::Summary,
        }
    }
}

/// MCP가 CLI `--level`과 같은 라벨·같은 대응으로 레벨을 해석하게 한다.
pub(crate) fn parse_level_label(label: &str) -> Option<Level> {
    LevelArg::from_str(label, false).ok().map(|arg| arg.level())
}

#[derive(Clone, ValueEnum)]
enum ReviewFormat {
    Json,
    Markdown,
    Sarif,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportFormat {
    Dbt,
    QueryLog,
}

#[derive(Clone, ValueEnum)]
enum LevelArg {
    Schema,
    Object,
    /// member 레벨의 CLI 이름. 컬럼이 의존성을 가지는 사실상 유일한 멤버다.
    Column,
    Member,
}

impl LevelArg {
    fn level(&self) -> Level {
        match self {
            LevelArg::Schema => Level::Schema,
            LevelArg::Object => Level::Object,
            LevelArg::Column | LevelArg::Member => Level::Member,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let code = match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    };
    std::process::exit(cancellation::exit_code(code));
}

async fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Import {
            catalog,
            format,
            input,
            output,
            report,
            project_root,
            max_input_bytes,
            max_rows,
            max_sql_bytes,
            max_total_sql_bytes,
        } => {
            let kind = match format {
                ImportFormat::Dbt => source::imports::ImportKind::DbtManifest,
                ImportFormat::QueryLog => source::imports::ImportKind::QueryLogJsonl,
            };
            import::run(
                &catalog,
                &input,
                &output,
                &report,
                kind,
                project_root.as_deref(),
                source::codec::LATEST_DOCUMENT_VERSION,
                source::imports::ImportLimits {
                    max_input_bytes,
                    max_rows: max_rows as usize,
                    max_sql_bytes,
                    max_total_sql_bytes,
                    ..Default::default()
                },
            )
        }
        Command::Scan {
            url,
            output,
            emit_document,
            document_version,
            document,
            inferred,
            source_id,
            schema,
            catalog_dependencies,
            sql_dir,
            query_schema,
            cache_dir,
        } => {
            scan(
                url.as_deref(),
                document.as_deref(),
                &output,
                emit_document.as_deref(),
                document_version.unwrap_or(if sql_dir.is_some() { 2 } else { 1 }),
                inferred,
                source_id.as_deref(),
                &schema,
                catalog_dependencies,
                sql_dir.as_deref(),
                query_schema.as_deref(),
                cache_dir.as_deref(),
            )
            .await
        }
        Command::Graph {
            graph,
            format,
            level,
        } => render_graph(&graph, format, level.map(|level| level.level())),
        Command::Search {
            pattern,
            graph,
            kind,
            detail,
            max,
        } => search(&pattern, &graph, kind.as_deref(), detail.detail(), max),
        Command::Query {
            name,
            graph,
            depth,
            max,
            max_visited,
            max_examined_edges,
        } => query(&name, &graph, depth, max, max_visited, max_examined_edges),
        Command::Impact {
            name,
            graph,
            max,
            max_visited,
            max_examined_edges,
        } => impact(&name, &graph, max, max_visited, max_examined_edges),
        Command::Dead {
            graph,
            max,
            strict,
            config,
            retain,
            as_of,
        } => {
            let policy = policy::load(config.as_deref(), &retain, as_of.as_deref())?;
            dead(&graph, max, strict, &policy)
        }
        Command::Unused { graph, max, strict } => unused(&graph, max, strict),
        Command::Diagnostics { graph, name } => diagnostics(&graph, name.as_deref()),
        Command::Explain { graph, name, max } => explain(&graph, &name, max),
        Command::Path {
            graph,
            from,
            to,
            max_paths,
            depth,
            max_visited,
            max_edges,
            reverse,
        } => path_report(
            &graph,
            &from,
            &to,
            analysis::paths::SearchOptions {
                max_paths,
                max_depth: depth,
                max_visited,
                max_edges,
                reverse,
            },
        ),
        Command::Cycles {
            graph,
            level,
            strict,
        } => cycles(&graph, level.level(), strict),
        Command::Stats { graph } => stats(&graph),
        Command::Diff { old, new, strict } => diff(&old, &new, strict),
        Command::Review {
            before,
            after,
            strict,
            require_complete,
            policy,
            baseline,
            as_of,
            write_baseline,
            max_changes,
            max_impacted,
            max_visited,
            max_examined_edges,
            format,
        } => review::run_with_options(review::ReviewOptions {
            before: &before,
            after: &after,
            strict,
            require_complete,
            max_changes,
            max_impacted,
            budget: analysis::budget::Budget {
                max_visited,
                max_examined_edges,
            },
            format: match format {
                ReviewFormat::Json => review::ReviewOutputFormat::Json,
                ReviewFormat::Markdown => review::ReviewOutputFormat::Markdown,
                ReviewFormat::Sarif => review::ReviewOutputFormat::Sarif,
            },
            policy: policy.as_deref(),
            baseline: baseline.as_deref(),
            as_of: as_of.as_deref(),
            write_baseline: write_baseline.as_deref(),
        }),
        Command::Serve {
            graph,
            config,
            retain,
            as_of,
        } => {
            // 정책은 운영자가 시작 시 명시한다 — 도구 인자나 실행 위치의 파일에 좌우되지 않는다.
            let retention = policy::load_explicit(config.as_deref(), &retain, as_of.as_deref())?;
            let graph = load_graph(&graph)?;
            let snapshot = mcp::Snapshot {
                graph: &graph,
                retention: &retention,
            };
            mcp::serve(
                &snapshot,
                BufReader::new(std::io::stdin()),
                std::io::stdout(),
            )?;
            Ok(0)
        }
        Command::Merge { documents, output } => {
            write_graph_output(&output, &merge::run(&documents)?)?;
            Ok(0)
        }
        Command::Facts {
            document,
            project,
            output,
        } => facts(&document, &project, output.as_deref()),
        Command::Openlineage {
            graph,
            namespace,
            database,
            event_time,
            output,
        } => openlineage(
            &graph,
            &namespace,
            database.as_deref(),
            event_time.as_deref(),
            output.as_deref(),
        ),
        Command::Lint { graph, max, strict } => {
            let graph = load_graph(&graph)?;
            let report = analysis::schema_lint::lint(&graph, max);
            println!(
                "{}",
                serde_json::to_string_pretty(&export::lint::to_value(&report))?
            );
            Ok(if strict && !report.complete {
                2
            } else if strict && report.blocking_count > 0 {
                1
            } else {
                0
            })
        }
        Command::Skill => {
            // 패키지 밖의 파일은 cargo install에서 사라지므로 사본을 포함한다.
            // 원본 skills/schemagraph/SKILL.md를 고치면 cli/SKILL.md도 갱신한다.
            print!("{}", include_str!("../SKILL.md"));
            Ok(0)
        }
        Command::DocumentCapabilities => {
            let value = serde_json::json!({
                "defaultVersion": source::document::DOCUMENT_VERSION,
                "supportedVersions": [1, source::codec::LATEST_DOCUMENT_VERSION],
                "supportedFeatures": source::codec::SUPPORTED_FEATURES,
                "formats": ["json", "ndjson"],
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Command::Rules {
            graph,
            config,
            strict,
        } => rules(&graph, &config, strict),
    }
}

async fn scan(
    url: Option<&str>,
    document: Option<&std::path::Path>,
    output: &str,
    emit_document: Option<&std::path::Path>,
    document_version: u32,
    inferred: bool,
    source_id: Option<&str>,
    schemas: &[String],
    catalog_dependencies: bool,
    sql_dir: Option<&std::path::Path>,
    query_schema: Option<&str>,
    cache_dir: Option<&std::path::Path>,
) -> Result<i32> {
    if let Some(id) = source_id {
        source::context::validate_source_id(id).map_err(|e| anyhow!(e))?;
    }
    let mut doc = match (url, document) {
        (Some(url), None) => if catalog_dependencies {
            source::read_with_dependencies(url).await
        } else {
            source::read(url).await
        }
        .context("database scan failed; check the connection settings and catalog permissions")?,
        (None, Some(path)) => load_document(path)?,
        (None, None) => bail!("URL 또는 --document 중 하나는 필요하다"),
        (Some(_), Some(_)) => bail!("URL과 --document는 같이 쓸 수 없다 — 둘 중 하나만"),
    };
    source::context::annotate(&mut doc, source_id, schemas).map_err(|e| anyhow!(e))?;
    if let Some(directory) = sql_dir {
        let schema = match query_schema {
            Some(schema) => schema.to_owned(),
            None if doc.schemas.len() == 1 => doc.schemas[0].name.clone(),
            None => {
                bail!("--query-schema is required when the catalog contains multiple or no schemas")
            }
        };
        source::sql_files::attach(&mut doc, directory, &schema).map_err(|e| anyhow!(e))?;
    }
    if let Some(path) = emit_document {
        let value = source::codec::document_to_value(&doc, document_version)
            .map_err(|error| anyhow!(error))?;
        let json = export::to_pretty_json(&value)?;
        std::fs::write(path, format!("{json}\n"))
            .with_context(|| format!("catalog document 쓰기 실패: {}", path.display()))?;
    }
    let mut cache = cache_dir
        .map(|directory| cache::DiskBodyCache::new(directory, &doc))
        .transpose()
        .context("could not prepare SQL analysis cache; check directory permissions")?;
    let graph = analyze_document_with_cache(
        &doc,
        inferred,
        cache
            .as_mut()
            .map(|cache| cache as &mut dyn schemagraph_parser::BodyCache),
    );
    if let Some(cache) = &cache {
        let stats = cache.stats();
        eprintln!(
            "schemagraph cache: hits={} misses={} writes={} warnings={}",
            stats.hits, stats.misses, stats.writes, stats.warnings
        );
    }
    // 그래프는 원문을 소유하지 않으므로 출력 전에 큰 카탈로그를 해제한다.
    drop(doc);
    write_graph_output(output, &graph)?;
    Ok(0)
}

fn analyze_document(doc: &source::CatalogDocument, inferred: bool) -> Graph {
    analyze_document_with_cache(doc, inferred, None)
}

fn analyze_document_with_cache(
    doc: &source::CatalogDocument,
    inferred: bool,
    cache: Option<&mut dyn schemagraph_parser::BodyCache>,
) -> Graph {
    let mut graph = source::graph::document_to_graph(doc);
    // 몸체 파싱은 엔진의 일 — reader는 원문만 옮기고 의미는 여기서 해석한다.
    let (_enriched, notes) = schemagraph_parser::enrich_with_cache(&mut graph, doc, cache);
    for note in notes {
        graph.add_limitation(note);
    }
    if inferred {
        // 이름 규칙 추정은 opt-in — 카탈로그·몸체 증거와 섞이지 않게
        // 별도 패스로 돌리고 한계도 그대로 limitations에 싣는다.
        let (_n, inotes) = schemagraph_parser::enrich_inferred(&mut graph, doc);
        for note in inotes {
            graph.add_limitation(note);
        }
    }
    graph
}

/// 카탈로그 문서를 isthmus bridge-facts 문서로 변환해 출력한다.
/// `project`는 호출 측 문서와 공유하는 realpath라 canonicalize로 정규화한다
/// — 다른 표기의 같은 경로가 조인에서 갈라지지 않게 한다.
fn facts(document: &Path, project: &Path, output: Option<&str>) -> Result<i32> {
    let doc = load_document(document)?;
    let project = std::fs::canonicalize(project).with_context(|| {
        format!(
            "project root를 읽을 수 없다: {} — 존재하는 디렉터리를 지정해라",
            project.display()
        )
    })?;
    let generated_at = rfc3339_utc_now()?;
    let value = source::bridge_facts::bridge_facts_document(
        &doc,
        project.to_string_lossy().as_ref(),
        env!("CARGO_PKG_VERSION"),
        &generated_at,
    );
    let json = export::to_pretty_json(&value)?;
    match output {
        Some(path) if path != "-" => {
            std::fs::write(path, format!("{json}\n"))
                .with_context(|| format!("bridge-facts 쓰기 실패: {path}"))?;
        }
        _ => println!("{json}"),
    }
    Ok(0)
}

/// 그래프 계보를 OpenLineage RunEvent NDJSON으로 쓴다.
///
/// 이벤트 시각을 고정하면 같은 그래프는 같은 파일이 된다(runId는 내용에서 만든다).
fn openlineage(
    path: &Path,
    namespace: &str,
    database: Option<&str>,
    event_time: Option<&str>,
    output: Option<&str>,
) -> Result<i32> {
    if namespace.trim().is_empty() {
        bail!(
            "--namespace must be nonempty; use the data source URI, such as postgres://host:5432"
        );
    }
    let event_time = match event_time {
        Some(time) => validate_rfc3339(time)?.to_owned(),
        None => rfc3339_utc_now()?,
    };
    let graph = load_graph(path)?;
    let result = lineage_events(&graph, namespace, database, &event_time);
    write_ndjson(&result.events, output)?;
    report_lineage_gaps(&graph, &result);
    Ok(0)
}

/// 계보 작업을 RunEvent로 만든다. 생산자와 facet 설명 URL은 이 도구 버전을 가리킨다.
fn lineage_events(
    graph: &Graph,
    namespace: &str,
    database: Option<&str>,
    event_time: &str,
) -> export::openlineage::Events {
    let version = env!("CARGO_PKG_VERSION");
    let producer = format!("https://github.com/ictechgy/schemagraph/tree/v{version}");
    let facet_url = format!(
        "https://github.com/ictechgy/schemagraph/blob/v{version}/ANALYSIS.md#export-lineage-to-openlineage"
    );
    let context = export::openlineage::EventContext {
        namespace,
        database,
        event_time,
        producer: &producer,
        analysis_facet_url: &facet_url,
    };
    let jobs = analysis::lineage::jobs(graph);
    export::openlineage::events(graph, &jobs, &context, content_run_id)
}

/// 이벤트를 한 줄에 하나씩 쓴다. `-`나 생략은 stdout이다.
fn write_ndjson(events: &[serde_json::Value], output: Option<&str>) -> Result<()> {
    let mut text = String::new();
    for event in events {
        text.push_str(&serde_json::to_string(event)?);
        text.push('\n');
    }
    match output {
        Some(path) if path != "-" => std::fs::write(path, text).with_context(|| {
            format!(
                "cannot write OpenLineage events to {path}; check the directory and permissions"
            )
        }),
        _ => {
            print!("{text}");
            Ok(())
        }
    }
}

/// 이벤트에 담지 못한 계보 공백을 stderr로 알린다 — 출력 파일은 결정적으로 남긴다.
fn report_lineage_gaps(graph: &Graph, result: &export::openlineage::Events) {
    let names = |ids: &[schemagraph_core::VertexId]| {
        ids.iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !result.indirect_omitted.is_empty() {
        eprintln!(
            "note: join/filter columns of {} routine job(s) were not exported because the graph does not record which statement a condition belongs to: {}",
            result.indirect_omitted.len(),
            names(&result.indirect_omitted)
        );
    }
    if !result.incomplete.is_empty() {
        eprintln!(
            "note: {} job(s) have incomplete SQL analysis and may miss lineage; see their schemagraphAnalysis job facet: {}",
            result.incomplete.len(),
            names(&result.incomplete)
        );
    }
    if !graph.limitations().is_empty() {
        eprintln!(
            "note: the graph records {} collection limitation(s); read graph.json limitations before treating missing lineage as absent",
            graph.limitations().len()
        );
    }
}

/// 이벤트 내용(runId 제외)의 SHA-256으로 RFC 9562 버전 8 UUID를 만든다.
///
/// 같은 내용은 같은 runId가 되어 재적재해도 소비자 쪽에서 중복 실행이 생기지 않는다.
/// serde_json 맵은 키가 정렬돼 있어 직렬화가 결정적이다.
fn content_run_id(event: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(event.to_string().as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// RFC 3339 날짜-시각인지 확인한다 — 필드 범위와 선택적 소수 초, `Z` 또는 `±HH:MM`.
///
/// 바이트 단위로만 읽어 비ASCII 입력에서도 패닉하지 않는다. 형식이 맞지 않는 값을
/// 싣으면 시각을 파싱하는 소비자가 이벤트 전체를 거부한다.
fn validate_rfc3339(value: &str) -> Result<&str> {
    let bytes = value.as_bytes();
    let date_time = bytes.len() >= 19 && rfc3339_date_time(&bytes[..19]);
    if date_time && rfc3339_zone(&bytes[19..]) {
        Ok(value)
    } else {
        bail!("--event-time must be RFC 3339, such as 2026-09-25T00:00:00Z; got '{value}'")
    }
}

/// 두 자리 십진수를 읽는다.
fn two_digits(high: u8, low: u8) -> Option<u32> {
    (high.is_ascii_digit() && low.is_ascii_digit())
        .then(|| u32::from(high - b'0') * 10 + u32::from(low - b'0'))
}

/// `YYYY-MM-DDTHH:MM:SS`의 구분자와 필드 범위를 검사한다.
fn rfc3339_date_time(b: &[u8]) -> bool {
    let in_range = |at: usize, low: u32, high: u32| {
        two_digits(b[at], b[at + 1]).is_some_and(|n| (low..=high).contains(&n))
    };
    b[..4].iter().all(u8::is_ascii_digit)
        && (b[4], b[7], b[13], b[16]) == (b'-', b'-', b':', b':')
        && matches!(b[10], b'T' | b't')
        && in_range(5, 1, 12)
        && in_range(8, 1, 31)
        && in_range(11, 0, 23)
        && in_range(14, 0, 59)
        && in_range(17, 0, 60)
}

/// 초 뒤의 선택적 소수부와 `Z`/`±HH:MM` 시간대를 검사한다.
fn rfc3339_zone(rest: &[u8]) -> bool {
    let fraction = match rest.first() {
        Some(b'.') => 1 + rest[1..].iter().take_while(|b| b.is_ascii_digit()).count(),
        _ => 0,
    };
    if fraction == 1 {
        return false;
    }
    match &rest[fraction..] {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => {
            two_digits(*h1, *h2).is_some_and(|h| h <= 23)
                && two_digits(*m1, *m2).is_some_and(|m| m <= 59)
        }
        _ => false,
    }
}

/// bridge-facts 계약이 요구하는 현재 시각의 RFC 3339 UTC 타임스탬프를 만든다.
fn rfc3339_utc_now() -> Result<String> {
    rfc3339_utc_at(std::time::SystemTime::now())
}

/// 주어진 시각을 RFC 3339 UTC로 만든다. 시계가 epoch 이전이면 실패한다 —
/// 1970년으로 대체하면 `generatedAt`이 관측이 아니라 거짓 값이 된다.
fn rfc3339_utc_at(time: std::time::SystemTime) -> Result<String> {
    let elapsed = time.duration_since(std::time::UNIX_EPOCH).map_err(|_| {
        anyhow!(
            "system clock is before 1970-01-01; cannot stamp generatedAt — fix the host clock and retry"
        )
    })?;
    // RFC 3339 연도는 네 자리라 9999년을 넘는 시각은 형식을 지킬 수 없다 —
    // 다섯 자리 연도를 싣지 않고 같은 원인으로 거절한다.
    let seconds = i64::try_from(elapsed.as_secs())
        .ok()
        .filter(|seconds| *seconds <= LAST_RFC3339_SECOND)
        .ok_or_else(|| {
            anyhow!(
                "system clock is beyond 9999-12-31; cannot stamp generatedAt — fix the host clock and retry"
            )
        })?;
    Ok(rfc3339_from_unix_seconds(seconds))
}

/// RFC 3339 네 자리 연도로 표현할 수 있는 마지막 Unix 초(9999-12-31T23:59:59Z)다.
const LAST_RFC3339_SECOND: i64 = 253_402_300_799;

/// Unix 초를 RFC 3339 UTC 문자열로 바꾼다.
/// 달력 변환은 외부 의존 없이 표준 civil 알고리즘으로 처리한다.
fn rfc3339_from_unix_seconds(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Unix 일수를 그레고리력 (year, month, day)로 변환한다 — Howard Hinnant의
/// civil_from_days 알고리즘이다.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

/// 프로브가 만든 catalog document를 읽는다. 단일 JSON과 NDJSON(행 단위)을
/// 자동으로 구분하고, 버전이 다르면 명확히 거절한다 — 조용히 읽으면 스키마가
/// 어긋난 채 그래프가 나와 소비자가 모른다.
fn load_document(path: &std::path::Path) -> Result<source::CatalogDocument> {
    let mut reader = open_reader(path)?;
    let probe = SnapshotProbe::deserialize(&mut serde_json::Deserializer::from_reader(&mut reader));
    reader.rewind().context("cannot rewind catalog input")?;
    // 잘못된 입력은 아래 실제 리더가 상세 오류를 내므로 형식 탐색 실패를 여기서 확정하지 않는다.
    if probe.is_ok_and(|probe| probe.kind.as_deref() == Some("document")) {
        return source::ndjson::document_from_reader(reader)
            .map_err(|error| anyhow!("invalid NDJSON catalog {}: {error}", path.display()));
    }
    source::codec::document_from_reader(reader)
        .map_err(|error| anyhow!("invalid catalog document {}: {error}", path.display()))
}

fn load_graph(path: &std::path::Path) -> Result<Graph> {
    let doc: GraphDoc = serde_json::from_reader(open_reader(path)?)
        .with_context(|| format!("graph.json 파싱 실패: {}", path.display()))?;
    if !matches!(doc.version, 1 | export::GRAPH_VERSION) {
        bail!(
            "graph.json 버전 {}는 이 바이너리({})와 다르다 — 다시 scan해라",
            doc.version,
            export::GRAPH_VERSION
        );
    }
    Ok(export::graph_from_doc(&doc))
}

fn render_graph(path: &std::path::Path, format: GraphFormat, level: Option<Level>) -> Result<i32> {
    let graph = load_graph(path)?;
    let projected = if matches!(&format, GraphFormat::Html) && level.is_none() {
        graph
    } else {
        graph.project(level.unwrap_or(Level::Object))
    };
    let out = match format {
        GraphFormat::Mermaid => export::mermaid::to_mermaid(&projected),
        GraphFormat::Dot => to_dot(&projected),
        GraphFormat::Json => {
            write_graph_output("-", &projected)?;
            return Ok(0);
        }
        GraphFormat::Html => {
            export::html::write_html(std::io::stdout().lock(), &projected)?;
            return Ok(0);
        }
    };
    println!("{out}");
    Ok(0)
}

/// graphviz dot 출력 — mermaid와 같은 규칙으로 의존 간선만 그린다.
fn to_dot(g: &Graph) -> String {
    let mut out = String::from("digraph schemagraph {\n    rankdir=LR;\n");
    for edge in g.edges() {
        if !edge.kind.is_dependency() {
            continue;
        }
        out.push_str(&format!(
            "    \"{}\" -> \"{}\" [label=\"{}\"];\n",
            edge.from.as_str(),
            edge.to.as_str(),
            export::edge_kind_str(edge.kind),
        ));
    }
    out.push_str("}\n");
    out
}

fn query(
    name: &str,
    path: &std::path::Path,
    depth: u32,
    max: usize,
    max_visited: usize,
    max_examined_edges: usize,
) -> Result<i32> {
    let cancel = cancellation::install()?;
    let graph = load_graph(path)?;
    match analysis::resolve(&graph, name) {
        Resolve::Found(id) => {
            let budget = analysis::budget::Budget {
                max_visited,
                max_examined_edges,
            };
            let dependents =
                analysis::budget::walk(&graph, &id, depth, max, true, budget, Some(cancel));
            let dependencies =
                analysis::budget::walk(&graph, &id, depth, max, false, budget, Some(cancel));
            let mut self_edges: Vec<_> = graph
                .outgoing(&id)
                .iter()
                .filter(|edge| edge.to == id && edge.kind.is_dependency())
                .map(|edge| edge.kind)
                .collect();
            self_edges.sort();
            self_edges.dedup();
            let value = export::budgeted_query_to_value(
                graph
                    .vertex(&id)
                    .expect("resolve verified the query subject"),
                &dependents,
                &dependencies,
                depth,
                &self_edges,
                graph.limitations(),
            );
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Resolve::NotFound { candidates } => {
            let value = export::not_found_value(name, &candidates, graph.limitations());
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(1)
        }
    }
}

/// impact — query와 같은 resolve 경로를 타고, 결과만 역방향 클로저로 다르다.
fn impact(
    name: &str,
    path: &std::path::Path,
    max: usize,
    max_visited: usize,
    max_examined_edges: usize,
) -> Result<i32> {
    let cancel = cancellation::install()?;
    let graph = load_graph(path)?;
    match analysis::resolve(&graph, name) {
        Resolve::Found(id) => {
            let report = analysis::budget::walk(
                &graph,
                &id,
                u32::MAX,
                max,
                true,
                analysis::budget::Budget {
                    max_visited,
                    max_examined_edges,
                },
                Some(cancel),
            );
            let value = export::budgeted_impact_to_value(
                graph
                    .vertex(&id)
                    .expect("resolve verified the impact subject"),
                &report,
                graph.limitations(),
            );
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Resolve::NotFound { candidates } => {
            let value = export::not_found_value(name, &candidates, graph.limitations());
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(1)
        }
    }
}

/// 이름 패턴으로 정점을 찾아 출력한다. 일치가 없어도 성공(0)이다 —
/// 빈 결과는 유효한 답이고, 수집 공백은 `limitations`로 따로 전달된다.
fn search(
    pattern: &str,
    path: &std::path::Path,
    kind: Option<&str>,
    detail: export::search::SearchDetail,
    max: usize,
) -> Result<i32> {
    let kind = parse_search_filters(pattern, kind)?;
    let graph = load_graph(path)?;
    let query = analysis::search::SearchQuery {
        pattern,
        kind,
        max,
        count_neighbors: detail == export::search::SearchDetail::Summary,
    };
    let report = analysis::search::search(&graph, query);
    let value = export::search::to_value(&report, pattern, kind, detail, graph.limitations());
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(0)
}

/// 검색 패턴과 종류 필터를 검증한다. CLI와 MCP가 같은 규칙으로 거절하도록 한곳에 둔다.
/// 종류 라벨은 패턴처럼 대소문자를 무시한다.
pub(crate) fn parse_search_filters(
    pattern: &str,
    kind: Option<&str>,
) -> Result<Option<schemagraph_core::VertexKind>> {
    if pattern.trim().is_empty() {
        bail!("search pattern must be nonempty; pass a name fragment or a glob such as 'public.*'");
    }
    kind.map(|kind| {
        export::vertex_kind_parse(&kind.to_ascii_lowercase()).ok_or_else(|| {
            anyhow!(
                "unknown vertex kind '{kind}'; use one of {}",
                export::VERTEX_KIND_LABELS.join(", ")
            )
        })
    })
    .transpose()
}

fn dead(
    path: &std::path::Path,
    max: usize,
    strict: bool,
    policy: &analysis::RetentionPolicy,
) -> Result<i32> {
    let graph = load_graph(path)?;
    let report = analysis::dead_with_policy(&graph, max, policy);
    println!(
        "{}",
        serde_json::to_string_pretty(&export::dead_to_value(&report))?
    );
    Ok(if strict && report.unsuppressed_count > 0 {
        1
    } else {
        0
    })
}

/// 통계 창 안에서 읽힌 기록이 없는 테이블·인덱스를 출력한다.
/// strict는 후보가 있으면 1이다 — 판정이 아니라 검토가 필요하다는 신호다.
fn unused(path: &std::path::Path, max: usize, strict: bool) -> Result<i32> {
    let graph = load_graph(path)?;
    let report = analysis::unused::unused(&graph, max);
    let value = export::unused::to_value(&report, graph.limitations());
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(if strict && report.total > 0 { 1 } else { 0 })
}

fn diagnostics(path: &std::path::Path, name: Option<&str>) -> Result<i32> {
    let graph = load_graph(path)?;
    let id = match name.map(|name| (name, analysis::resolve(&graph, name))) {
        Some((_, Resolve::Found(id))) => Some(id),
        Some((name, Resolve::NotFound { candidates })) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&export::not_found_value(
                    name,
                    &candidates,
                    graph.limitations()
                ))?
            );
            return Ok(1);
        }
        None => None,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&export::diagnostics::report(&graph, id.as_ref()))?
    );
    Ok(0)
}

fn explain(path: &std::path::Path, name: &str, max: usize) -> Result<i32> {
    let graph = load_graph(path)?;
    match analysis::resolve(&graph, name) {
        Resolve::Found(id) => {
            let report = analysis::paths::explain(&graph, &id, max)
                .expect("resolve verified the subject exists");
            println!(
                "{}",
                serde_json::to_string_pretty(&export::explain::explanation_value(&report, &graph))?
            );
            Ok(0)
        }
        Resolve::NotFound { candidates } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&export::not_found_value(
                    name,
                    &candidates,
                    graph.limitations()
                ))?
            );
            Ok(1)
        }
    }
}

fn path_report(
    path: &std::path::Path,
    from: &str,
    to: &str,
    options: analysis::paths::SearchOptions,
) -> Result<i32> {
    let cancel = cancellation::install()?;
    let graph = load_graph(path)?;
    let mut endpoints = Vec::new();
    for name in [from, to] {
        match analysis::resolve(&graph, name) {
            Resolve::Found(id) => endpoints.push(id),
            Resolve::NotFound { candidates } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&export::not_found_value(
                        name,
                        &candidates,
                        graph.limitations()
                    ))?
                );
                return Ok(1);
            }
        }
    }
    let report =
        analysis::paths::paths(&graph, &endpoints[0], &endpoints[1], options, Some(cancel));
    println!(
        "{}",
        serde_json::to_string_pretty(&export::explain::path_value(&report))?
    );
    Ok(0)
}

fn stats(path: &std::path::Path) -> Result<i32> {
    let graph = load_graph(path)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&export::stats_to_value(&graph))?
    );
    Ok(0)
}

/// 입력 파일이 graph.json인지 catalog document인지 꼴로 판별한다 —
/// 두 산출물은 계약이 달라 같은 명령으로 비교할 수 없다.
fn snapshot_kind(path: &std::path::Path) -> Result<SnapshotKind> {
    let probe = SnapshotProbe::deserialize(&mut serde_json::Deserializer::from_reader(
        open_reader(path)?,
    ))
    .with_context(|| format!("invalid snapshot {}", path.display()))?;
    if probe.kind.as_deref() == Some("document") {
        return Ok(SnapshotKind::Document);
    }
    if probe.vertices.is_some() && probe.edges.is_some() {
        Ok(SnapshotKind::Graph)
    } else if probe.schemas.is_some() {
        Ok(SnapshotKind::Document)
    } else {
        bail!(
            "스냅샷 형태를 모르겠다: {} — graph.json(vertices/edges) 또는 catalog document(schemas)여야 한다",
            path.display()
        )
    }
}

enum SnapshotKind {
    Graph,
    Document,
}

// 큰 배열·몸체는 IgnoredAny로 건너뛰고 형식 구분에 필요한 키만 보관한다.
#[derive(Deserialize)]
struct SnapshotProbe {
    #[serde(rename = "type")]
    kind: Option<String>,
    vertices: Option<serde::de::IgnoredAny>,
    edges: Option<serde::de::IgnoredAny>,
    schemas: Option<serde::de::IgnoredAny>,
}

fn open_reader(path: &std::path::Path) -> Result<BufReader<std::fs::File>> {
    let file = std::fs::File::open(path).with_context(|| {
        format!(
            "cannot open input {}; check the path and permissions",
            path.display()
        )
    })?;
    Ok(BufReader::new(file))
}

/// diff — 같은 종류의 산출물 둘을 비교한다. usage는 델타에서 뺀다 —
/// 카운트·시각은 관측 부속물이라 항상 달라 스키마 델타를 묻는다.
fn diff(old_path: &std::path::Path, new_path: &std::path::Path, strict: bool) -> Result<i32> {
    let (old_kind, new_kind) = (snapshot_kind(old_path)?, snapshot_kind(new_path)?);
    if std::mem::discriminant(&old_kind) != std::mem::discriminant(&new_kind) {
        bail!("graph.json과 catalog document는 비교할 수 없다 — 같은 종류끼리 diff해라");
    }
    let (value, changed) = match old_kind {
        SnapshotKind::Graph => {
            let report = analysis::diff_graph(&load_graph(old_path)?, &load_graph(new_path)?);
            let changed = !report.vertices_added.is_empty()
                || !report.vertices_removed.is_empty()
                || !report.vertices_changed.is_empty()
                || !report.edges_added.is_empty()
                || !report.edges_removed.is_empty();
            (export::graph_diff_to_value(&report), changed)
        }
        SnapshotKind::Document => {
            let report =
                source::diff::diff_documents(&load_document(old_path)?, &load_document(new_path)?);
            let changed =
                report.summary.added + report.summary.removed + report.summary.changed > 0;
            (serde_json::to_value(&report)?, changed)
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(if strict && changed { 1 } else { 0 })
}

fn cycles(path: &std::path::Path, level: Level, strict: bool) -> Result<i32> {
    let graph = load_graph(path)?;
    let report = analysis::cycles(&graph, level);
    println!(
        "{}",
        serde_json::to_string_pretty(&export::cycles_to_value(&report))?
    );
    Ok(if strict && !report.cycles.is_empty() {
        1
    } else {
        0
    })
}

/// TOML 규칙 파일의 형태 — [[rule]] 테이블 목록. kinds를 빼면 모든
/// 의존성 간선을 대상으로 한다(Contains·Inferred는 어차피 판정 밖).
#[derive(serde::Deserialize)]
struct RulesFile {
    #[serde(default)]
    rule: Vec<RuleToml>,
}

/// 규칙 하나의 파일 표현 — name/from/to는 필수, kinds는 선택.
#[derive(serde::Deserialize)]
struct RuleToml {
    name: String,
    from: String,
    to: String,
    kinds: Option<Vec<String>>,
}

/// 규칙 파일을 analysis::Rule로 변환한다. 모르는 간선 종류는 명확한
/// 오류로 — 조용히 무시하면 규칙이 느슨해지는데 사용자는 엄격해졌다고 믿는다.
fn load_rules(path: &std::path::Path) -> Result<Vec<analysis::Rule>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("규칙 파일을 못 읽음: {}", path.display()))?;
    let file: RulesFile = toml::from_str(&text)
        .with_context(|| format!("규칙 파일 TOML 파싱 실패: {}", path.display()))?;
    file.rule
        .iter()
        .map(|r| {
            let kinds = r
                .kinds
                .as_ref()
                .map(|ks| {
                    ks.iter()
                        .map(|k| {
                            export::edge_kind_parse(k).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "규칙 '{}': 알 수 없는 간선 종류 '{k}' — \
                                     references|reads|writes|calls|fires|uses-sequence|uses-type",
                                    r.name
                                )
                            })
                        })
                        .collect::<Result<std::collections::BTreeSet<_>>>()
                })
                .transpose()?;
            Ok(analysis::Rule {
                name: r.name.clone(),
                from: r.from.clone(),
                to: r.to.clone(),
                kinds,
            })
        })
        .collect()
}

fn rules(path: &std::path::Path, config: &std::path::Path, strict: bool) -> Result<i32> {
    let graph = load_graph(path)?;
    let rule_set = load_rules(config)?;
    let report = analysis::rules(&graph, &rule_set);
    println!(
        "{}",
        serde_json::to_string_pretty(&export::rules_to_value(&report))?
    );
    Ok(if strict && !report.violations.is_empty() {
        1
    } else {
        0
    })
}

fn write_graph_output(output: &str, graph: &Graph) -> Result<()> {
    fn write(mut writer: impl Write, graph: &Graph) -> Result<()> {
        export::stream::write_graph(&mut writer, graph)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
    if output == "-" {
        write(BufWriter::new(std::io::stdout().lock()), graph)
    } else {
        let file = std::fs::File::create(output).with_context(|| {
            format!("cannot create graph output {output}; check the directory and permissions")
        })?;
        write(BufWriter::new(file), graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 기대값은 Python `datetime.fromtimestamp(s, timezone.utc)`로 독립 계산했다 —
    /// 엔진 출력에서 역산하면 달력 변환 오류가 기대값에 그대로 복사된다.
    #[test]
    fn rfc3339_formats_epoch_leap_and_century_boundaries() {
        assert_eq!(rfc3339_from_unix_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_from_unix_seconds(951_782_400),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            rfc3339_from_unix_seconds(1_790_207_999),
            "2026-09-23T23:59:59Z"
        );
        // 2100년은 윤년이 아니다 — 2월 28일 다음이 3월 1일이어야 한다.
        assert_eq!(
            rfc3339_from_unix_seconds(4_107_542_400),
            "2100-03-01T00:00:00Z"
        );
    }

    /// 시계가 1970년 이전이면 조용히 epoch 시각을 싣지 않고 원인을 담아 실패한다.
    #[test]
    fn clock_before_epoch_is_an_error_not_a_1970_timestamp() {
        let before_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
        let error = rfc3339_utc_at(before_epoch).unwrap_err().to_string();
        assert!(error.contains("system clock"), "{error}");
    }

    /// RFC 3339 연도는 네 자리다 — 9999년 끝까지는 받고 그 다음 초부터는 거절한다.
    /// 경계값 253_402_300_799는 Python `datetime(9999,12,31,23,59,59, UTC)`로 계산했다.
    #[test]
    fn clock_beyond_year_9999_is_an_error_not_a_five_digit_year() {
        let last = std::time::UNIX_EPOCH + std::time::Duration::from_secs(253_402_300_799);
        assert_eq!(rfc3339_utc_at(last).unwrap(), "9999-12-31T23:59:59Z");
        let beyond = last + std::time::Duration::from_secs(1);
        let error = rfc3339_utc_at(beyond).unwrap_err().to_string();
        assert!(error.contains("system clock"), "{error}");
    }

    /// runId는 내용이 같으면 같고, RFC 9562 버전 8·변형 비트를 가진 UUID다.
    #[test]
    fn content_run_id_is_a_deterministic_version_8_uuid() {
        let event = serde_json::json!({"job": {"name": "s.report"}});
        let id = content_run_id(&event);
        assert_eq!(id, content_run_id(&event.clone()));
        assert_ne!(
            id,
            content_run_id(&serde_json::json!({"job": {"name": "s.other"}}))
        );
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('8'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }

    /// 이벤트 시각은 RFC 3339 형태만 받는다.
    #[test]
    fn event_time_accepts_rfc3339_only() {
        for good in ["2026-09-25T00:00:00Z", "2026-09-25T09:00:00.5+09:00"] {
            assert!(validate_rfc3339(good).is_ok(), "{good}");
        }
        for bad in [
            "2026-09-25",
            "2026-09-25 00:00:00Z",
            "yesterday",
            "2026-09-25T00:00:00",
            "2026-13-45T99:99:99garbageZ",
            "2026-09-25T00:00:00+ab:cd",
            "2026-09-25T00:00:00.Z",
            "é12345",
            "2026-09-25T00:00:00é12345",
        ] {
            assert!(validate_rfc3339(bad).is_err(), "{bad}");
        }
    }
}
