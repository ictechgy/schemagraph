//! schemagraph CLI — scan으로 그래프를 만들고, 질의는 graph.json 위에서 한다.
//!
//! 종료 코드 계약:
//! - 0: 성공 (또는 보고할 것이 없음)
//! - 1: 질의가 발견을 보고함(--strict 시) 또는 대상을 못 찾음(notFound)
//! - 2: 사용법·엔진 오류 (잘못된 URL, 파일 없음, 파싱 불가)

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use schemagraph_analysis::{self as analysis, Resolve};
use schemagraph_core::{Graph, Level};
use schemagraph_export::{self as export, GraphDoc};
use schemagraph_source::{self as source};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "schemagraph",
    version,
    about = "Build a dependency graph of a database schema and run judgment queries on it"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// Build the graph from a catalog document (probe output) instead of a live URL.
        #[arg(long)]
        document: Option<PathBuf>,
    },
    /// Render the graph as mermaid/json/dot.
    Graph {
        /// Input graph.json produced by `scan`.
        #[arg(short, long, default_value = "graph.json")]
        graph: PathBuf,
        #[arg(long, value_enum, default_value_t = GraphFormat::Json)]
        format: GraphFormat,
        /// Aggregation level (column = member level).
        #[arg(long, value_enum, default_value_t = LevelArg::Object)]
        level: LevelArg,
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
    /// Print the agent skill document for consuming this tool's output.
    Skill,
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
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Scan {
            url,
            output,
            emit_document,
            document,
        } => {
            scan(
                url.as_deref(),
                document.as_deref(),
                &output,
                emit_document.as_deref(),
            )
            .await
        }
        Command::Graph {
            graph,
            format,
            level,
        } => render_graph(&graph, format, level.level()),
        Command::Query {
            name,
            graph,
            depth,
            max,
        } => query(&name, &graph, depth, max),
        Command::Impact { name, graph, max } => impact(&name, &graph, max),
        Command::Dead { graph, max, strict } => dead(&graph, max, strict),
        Command::Cycles {
            graph,
            level,
            strict,
        } => cycles(&graph, level.level(), strict),
        Command::Stats { graph } => stats(&graph),
        Command::Skill => {
            // 스킬 원본은 저장소의 skills/schemagraph/SKILL.md — 에이전트가
            // 저장소에서 직접 읽을 수도 있고 이 명령으로 설치할 수도 있다.
            print!("{}", include_str!("../../../skills/schemagraph/SKILL.md"));
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
) -> Result<i32> {
    let doc = match (url, document) {
        (Some(url), None) => source::read(url)
            .await
            .with_context(|| format!("스캔 실패: {url}"))?,
        (None, Some(path)) => load_document(path)?,
        (None, None) => bail!("URL 또는 --document 중 하나는 필요하다"),
        (Some(_), Some(_)) => bail!("URL과 --document는 같이 쓸 수 없다 — 둘 중 하나만"),
    };
    if let Some(path) = emit_document {
        let json = export::to_pretty_json(&doc)?;
        std::fs::write(path, format!("{json}\n"))
            .with_context(|| format!("catalog document 쓰기 실패: {}", path.display()))?;
    }
    let mut graph = source::graph::document_to_graph(&doc);
    // 몸체 파싱은 엔진의 일 — reader는 원문만 옮기고 의미는 여기서 해석한다.
    let (_enriched, notes) = schemagraph_parser::enrich_from_document(&mut graph, &doc);
    for note in notes {
        graph.add_limitation(note);
    }
    let graph_doc = export::graph_to_doc(&graph);
    let json = export::to_pretty_json(&graph_doc)?;
    write_output(output, &json)?;
    Ok(0)
}

/// 프로브가 만든 catalog document를 읽는다. 버전이 다르면 명확히 거절한다 —
/// 조용히 읽으면 스키마가 어긋난 채 그래프가 나와 소비자가 모른다.
fn load_document(path: &std::path::Path) -> Result<source::CatalogDocument> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("catalog document를 못 읽음: {}", path.display()))?;
    let doc: source::CatalogDocument = serde_json::from_str(&text)
        .with_context(|| format!("catalog document 파싱 실패: {}", path.display()))?;
    if doc.version != source::document::DOCUMENT_VERSION {
        bail!(
            "catalog document 버전 {}는 이 바이너리({})와 다르다 — 프로브 버전을 확인해라",
            doc.version,
            source::document::DOCUMENT_VERSION
        );
    }
    Ok(doc)
}

fn load_graph(path: &std::path::Path) -> Result<Graph> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("graph.json을 못 읽음: {}", path.display()))?;
    let doc: GraphDoc = serde_json::from_str(&text)
        .with_context(|| format!("graph.json 파싱 실패: {}", path.display()))?;
    if doc.version != export::GRAPH_VERSION {
        bail!(
            "graph.json 버전 {}는 이 바이너리({})와 다르다 — 다시 scan해라",
            doc.version,
            export::GRAPH_VERSION
        );
    }
    Ok(export::graph_from_doc(&doc))
}

fn render_graph(path: &std::path::Path, format: GraphFormat, level: Level) -> Result<i32> {
    let graph = load_graph(path)?;
    let projected = graph.project(level);
    let out = match format {
        GraphFormat::Mermaid => export::mermaid::to_mermaid(&projected),
        GraphFormat::Dot => to_dot(&projected),
        GraphFormat::Json => {
            let doc = export::graph_to_doc(&projected);
            export::to_pretty_json(&doc)?
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

fn query(name: &str, path: &std::path::Path, depth: u32, max: usize) -> Result<i32> {
    let graph = load_graph(path)?;
    match analysis::resolve(&graph, name) {
        Resolve::Found(id) => {
            let report = analysis::query(&graph, &id, depth, max);
            println!(
                "{}",
                serde_json::to_string_pretty(&export::query_to_value(&report))?
            );
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
fn impact(name: &str, path: &std::path::Path, max: usize) -> Result<i32> {
    let graph = load_graph(path)?;
    match analysis::resolve(&graph, name) {
        Resolve::Found(id) => {
            let report = analysis::impact(&graph, &id, max);
            println!(
                "{}",
                serde_json::to_string_pretty(&export::impact_to_value(&report))?
            );
            Ok(0)
        }
        Resolve::NotFound { candidates } => {
            let value = export::not_found_value(name, &candidates, graph.limitations());
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(1)
        }
    }
}

fn dead(path: &std::path::Path, max: usize, strict: bool) -> Result<i32> {
    let graph = load_graph(path)?;
    let report = analysis::dead(&graph, max);
    println!(
        "{}",
        serde_json::to_string_pretty(&export::dead_to_value(&report))?
    );
    Ok(if strict && !report.candidates.is_empty() {
        1
    } else {
        0
    })
}

fn stats(path: &std::path::Path) -> Result<i32> {
    let graph = load_graph(path)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&export::stats_to_value(&graph))?
    );
    Ok(0)
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

fn write_output(output: &str, json: &str) -> Result<()> {
    if output == "-" {
        println!("{json}");
    } else {
        std::fs::write(output, format!("{json}\n"))
            .with_context(|| format!("출력 쓰기 실패: {output}"))?;
    }
    Ok(())
}
