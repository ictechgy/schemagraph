//! 오프라인에서 여는 HTML 그래프 탐색기.
//!
//! 문서는 이스케이프한 JSON으로 넣고 브라우저는 선택한 정점의 제한된 이웃만
//! 그린다. 그래프 표현을 하나 더 만들지 않고도 큰 그래프의 첫 화면을 가볍게
//! 유지한다.

use std::io::{self, Write};

use schemagraph_core::Graph;

use crate::{graph_to_doc, GraphDoc};

/// 오프라인에서 열 수 있는 단일 파일 그래프 탐색기를 쓴다.
///
/// 그래프 문서가 유일한 사실의 원천이다. 이름·근거·원문 출처·분석 기록은
/// DOM의 `textContent`로 데이터로 출력하고 첫 화면에는 요약만 표시한다.
pub fn write_html(mut writer: impl Write, graph: &Graph) -> io::Result<()> {
    let mut document = graph_to_doc(graph);
    canonicalize_document(&mut document);
    let json = serde_json::to_string(&document)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let safe_json = escape_script_json(&json);

    writer.write_all(HTML_PREFIX.as_bytes())?;
    writer.write_all(safe_json.as_bytes())?;
    writer.write_all(HTML_SUFFIX.as_bytes())
}

/// 호출자가 근거를 다른 순서로 조립했어도 배열 필드의 출력 순서를 고정한다.
fn canonicalize_document(document: &mut GraphDoc) {
    document.vertices.sort_by(|a, b| a.id.cmp(&b.id));
    document
        .edges
        .sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    for edge in &mut document.edges {
        edge.evidence
            .sort_by(|a, b| (&a.layer, &a.detail).cmp(&(&b.layer, &b.detail)));
    }
    document.analysis.sort_by(|a, b| a.id.cmp(&b.id));
    for analysis in &mut document.analysis {
        analysis.diagnostics.sort_by(|a, b| {
            (&a.code, &a.message, location_key(a.location.as_ref())).cmp(&(
                &b.code,
                &b.message,
                location_key(b.location.as_ref()),
            ))
        });
    }
    document.origins.sort_by(|a, b| {
        (
            &a.from,
            &a.to,
            &a.kind,
            &a.body_hash,
            &a.role,
            location_key(a.location.as_ref()),
        )
            .cmp(&(
                &b.from,
                &b.to,
                &b.kind,
                &b.body_hash,
                &b.role,
                location_key(b.location.as_ref()),
            ))
    });
}

fn location_key(location: Option<&crate::diagnostics::LocationDoc>) -> (u64, u64, u64, u64) {
    location
        .map(|location| {
            (
                location.line,
                location.column,
                location.end_line,
                location.end_column,
            )
        })
        .unwrap_or((0, 0, 0, 0))
}

/// `<`, `>`, `&`를 이스케이프해 JSON.parse 전에 HTML 파싱이나 `</script>` 종료가
/// 일어나지 않게 한다.
fn escape_script_json(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            _ => escaped.push(character),
        }
    }
    escaped
}

const HTML_PREFIX: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SchemaGraph Explorer</title>
<style>
:root { color-scheme: light dark; --bg: #f7f8fa; --panel: #fff; --ink: #20252b; --muted: #68727d; --line: #d9dee5; --accent: #2864c7; --warn: #a35a00; }
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 15px/1.45 system-ui, sans-serif; }
main { max-width: 1180px; margin: 0 auto; padding: 28px 20px 56px; }
h1, h2, h3 { line-height: 1.2; }
h1 { margin: 0 0 6px; font-size: 28px; }
h2 { margin: 28px 0 12px; font-size: 20px; }
h3 { margin: 0 0 8px; font-size: 16px; }
p { margin: 6px 0; }
.muted { color: var(--muted); }
.panel { margin-top: 18px; padding: 16px; border: 1px solid var(--line); border-radius: 9px; background: var(--panel); }
.summary { display: flex; flex-wrap: wrap; gap: 8px 18px; color: var(--muted); }
.search-row { display: flex; gap: 10px; align-items: center; }
input[type=search] { width: min(680px, 100%); padding: 10px 12px; border: 1px solid var(--line); border-radius: 7px; color: inherit; background: var(--bg); font: inherit; }
button { border: 0; padding: 0; color: var(--accent); background: transparent; font: inherit; text-align: left; cursor: pointer; }
button:hover, button:focus-visible { text-decoration: underline; }
.results { display: grid; gap: 6px; margin-top: 12px; }
.result { display: flex; justify-content: space-between; gap: 12px; padding: 8px 10px; border: 1px solid var(--line); border-radius: 6px; }
.result-meta, .edge-meta, .tag { color: var(--muted); font-size: 13px; }
.tag { display: inline-block; margin-right: 6px; padding: 2px 6px; border: 1px solid var(--line); border-radius: 4px; }
.partial { color: var(--warn); }
.detail-head { display: flex; flex-wrap: wrap; justify-content: space-between; gap: 8px 20px; align-items: baseline; }
.edge-list { display: grid; gap: 10px; }
.edge-card { padding: 12px; border: 1px solid var(--line); border-radius: 7px; }
.edge-title { display: flex; flex-wrap: wrap; gap: 6px 12px; align-items: baseline; }
.edge-title strong { color: var(--accent); }
.evidence, .origins, .diagnostics { margin: 9px 0 0; padding-left: 20px; }
.evidence li, .origins li, .diagnostics li { margin: 4px 0; }
.empty { color: var(--muted); }
@media (prefers-color-scheme: dark) {
  :root { --bg: #17191d; --panel: #202329; --ink: #edf0f3; --muted: #aeb7c2; --line: #3b424c; --accent: #8cb4ff; --warn: #f0b45f; }
}
</style>
</head>
<body>
<main>
<header>
<h1>SchemaGraph Explorer</h1>
<p class="muted">An offline view of the catalog graph. Search for an object to inspect its direct neighbors and evidence.</p>
<p id="summary" class="summary"></p>
</header>
<section class="panel" aria-labelledby="search-heading">
<h2 id="search-heading">Find an object</h2>
<div class="search-row">
<label for="search">Name, id, schema, or kind</label>
<input id="search" type="search" autocomplete="off" placeholder="Try orders or public.customer">
</div>
<p id="search-note" class="muted">The initial view does not render the whole graph. Type to search.</p>
<div id="results" class="results" aria-live="polite"></div>
</section>
<section id="details" class="panel" aria-labelledby="details-heading" hidden>
<div class="detail-head"><h2 id="details-heading">Selected object</h2><span id="selected-kind" class="tag"></span></div>
<p id="selected-id" class="muted"></p>
<div id="analysis"></div>
<h3>Immediate neighbors</h3>
<p id="neighbor-note" class="muted"></p>
<div id="neighbors" class="edge-list"></div>
</section>
<section id="limitations" class="panel" aria-labelledby="limitations-heading" hidden>
<h2 id="limitations-heading">Recorded limitations</h2>
<ul id="limitation-list"></ul>
</section>
</main>
<script id="graph-data" type="application/json">"##;

const HTML_SUFFIX: &str = r##"</script>
<script>
(() => {
  "use strict";

  const graph = JSON.parse(document.getElementById("graph-data").textContent);
  const MAX_RESULTS = 30;
  const MAX_NEIGHBORS = 100;
  const vertices = new Map(graph.vertices.map((vertex) => [vertex.id, vertex]));
  const summary = document.getElementById("summary");
  const search = document.getElementById("search");
  const searchNote = document.getElementById("search-note");
  const results = document.getElementById("results");
  const details = document.getElementById("details");
  const selectedKind = document.getElementById("selected-kind");
  const selectedId = document.getElementById("selected-id");
  const analysis = document.getElementById("analysis");
  const neighborNote = document.getElementById("neighbor-note");
  const neighbors = document.getElementById("neighbors");
  const limitations = document.getElementById("limitations");
  const limitationList = document.getElementById("limitation-list");

  appendText(summary, `${graph.vertices.length} objects`);
  appendText(summary, `${graph.edges.length} edges`);
  appendText(summary, `${graph.analysis.length} analysis records`);
  appendText(summary, `${graph.origins.length} edge origins`);
  if (graph.limitations.length > 0) {
    appendText(summary, `${graph.limitations.length} recorded limitations`);
    limitations.hidden = false;
    for (const limitation of graph.limitations) {
      const item = document.createElement("li");
      item.textContent = limitation;
      limitationList.appendChild(item);
    }
  }

  search.addEventListener("input", renderSearch);
  renderSearch();

  function appendText(parent, value, className) {
    const element = document.createElement("span");
    if (className) element.className = className;
    element.textContent = value;
    parent.appendChild(element);
  }

  function renderSearch() {
    results.replaceChildren();
    const query = search.value.trim().toLocaleLowerCase();
    if (!query) {
      searchNote.textContent = "The initial view does not render the whole graph. Type to search.";
      return;
    }
    const matches = graph.vertices.filter((vertex) =>
      [vertex.id, vertex.name, vertex.schema, vertex.kind].some((value) =>
        value.toLocaleLowerCase().includes(query)
      )
    );
    const shown = matches.slice(0, MAX_RESULTS);
    searchNote.textContent = matches.length > MAX_RESULTS
      ? `Showing ${MAX_RESULTS} of ${matches.length} matches. Refine the search to see more.`
      : `${matches.length} match${matches.length === 1 ? "" : "es"}.`;
    if (matches.length > MAX_RESULTS) searchNote.className = "partial";
    else searchNote.className = "muted";
    if (shown.length === 0) {
      const empty = document.createElement("p");
      empty.className = "empty";
      empty.textContent = "No matching object.";
      results.appendChild(empty);
      return;
    }
    for (const vertex of shown) {
      const row = document.createElement("div");
      row.className = "result";
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = vertex.name || vertex.id;
      button.title = vertex.id;
      button.addEventListener("click", () => selectVertex(vertex.id));
      row.appendChild(button);
      const meta = document.createElement("span");
      meta.className = "result-meta";
      meta.textContent = `${vertex.kind} · ${vertex.schema || "(no schema)"}`;
      row.appendChild(meta);
      results.appendChild(row);
    }
  }

  function selectVertex(id) {
    const vertex = vertices.get(id);
    if (!vertex) return;
    details.hidden = false;
    selectedKind.textContent = `${vertex.kind} · ${vertex.level}`;
    selectedId.textContent = vertex.id;
    renderAnalysis(vertex.id);
    renderNeighbors(vertex.id);
    details.scrollIntoView({ block: "start" });
  }

  function renderAnalysis(id) {
    analysis.replaceChildren();
    const record = graph.analysis.find((item) => item.id === id);
    if (!record) {
      const empty = document.createElement("p");
      empty.className = "muted";
      empty.textContent = "No object-level analysis record is available.";
      analysis.appendChild(empty);
      return;
    }
    const heading = document.createElement("p");
    appendText(heading, "Analysis: ");
    appendText(heading, record.state, `tag state-${record.state}`);
    appendText(heading, `scope: ${record.scope}`, "tag");
    if (record.body_hash) appendText(heading, `body hash: ${record.body_hash}`, "tag");
    analysis.appendChild(heading);
    if (record.diagnostics.length === 0) return;
    const list = document.createElement("ul");
    list.className = "diagnostics";
    for (const diagnostic of record.diagnostics) {
      const item = document.createElement("li");
      const location = diagnostic.location ? ` (${formatLocation(diagnostic.location)})` : "";
      item.textContent = `${diagnostic.code}: ${diagnostic.message}${location}`;
      list.appendChild(item);
    }
    analysis.appendChild(list);
  }

  function renderNeighbors(id) {
    neighbors.replaceChildren();
    const incident = graph.edges
      .filter((edge) => edge.from === id || edge.to === id)
      .map((edge) => ({
        edge,
        direction: edge.from === id && edge.to === id ? "self" : edge.from === id ? "outgoing" : "incoming",
        neighbor: edge.from === id ? edge.to : edge.from,
      }))
      .sort((left, right) =>
        [left.direction, left.neighbor, left.edge.kind, left.edge.from, left.edge.to]
          .join("\u0000")
          .localeCompare([right.direction, right.neighbor, right.edge.kind, right.edge.from, right.edge.to].join("\u0000"))
      );
    const shown = incident.slice(0, MAX_NEIGHBORS);
    neighborNote.textContent = incident.length > MAX_NEIGHBORS
      ? `Showing ${MAX_NEIGHBORS} of ${incident.length} incident edges. Refine the graph document or inspect a neighboring object for more context.`
      : `${incident.length} incident edge${incident.length === 1 ? "" : "s"}.`;
    neighborNote.className = incident.length > MAX_NEIGHBORS ? "partial" : "muted";
    if (shown.length === 0) {
      const empty = document.createElement("p");
      empty.className = "empty";
      empty.textContent = "No immediate neighbors are recorded.";
      neighbors.appendChild(empty);
      return;
    }
    for (const item of shown) renderEdge(item, id);
  }

  function renderEdge(item, selectedId) {
    const edge = item.edge;
    const card = document.createElement("article");
    card.className = "edge-card";
    const title = document.createElement("div");
    title.className = "edge-title";
    appendText(title, item.direction, "tag");
    appendText(title, edge.kind, "tag");
    const neighbor = vertices.get(item.neighbor);
    if (neighbor) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = neighbor.name || neighbor.id;
      button.title = neighbor.id;
      button.addEventListener("click", () => selectVertex(neighbor.id));
      title.appendChild(button);
      appendText(title, `(${neighbor.kind})`, "edge-meta");
    } else {
      appendText(title, `${item.neighbor} (missing vertex record)`, "edge-meta");
    }
    card.appendChild(title);
    const endpoints = document.createElement("p");
    endpoints.className = "edge-meta";
    endpoints.textContent = `${edge.from} → ${edge.to}`;
    card.appendChild(endpoints);
    addEvidence(card, edge.evidence);
    const origins = graph.origins.filter((origin) =>
      origin.from === edge.from && origin.to === edge.to && origin.kind === edge.kind
    );
    addOrigins(card, origins);
    // 표시하는 카드는 모두 문서의 접점 간선에서만 왔는지 바로 확인한다.
    // 브라우저가 관계를 추론해 간선을 만들어서는 안 된다.
    if (edge.from !== selectedId && edge.to !== selectedId) return;
    neighbors.appendChild(card);
  }

  function addEvidence(parent, evidence) {
    if (!evidence || evidence.length === 0) return;
    const heading = document.createElement("p");
    heading.className = "edge-meta";
    heading.textContent = "Evidence";
    parent.appendChild(heading);
    const list = document.createElement("ul");
    list.className = "evidence";
    for (const item of evidence) {
      const row = document.createElement("li");
      row.textContent = `${item.layer}: ${item.detail}`;
      list.appendChild(row);
    }
    parent.appendChild(list);
  }

  function addOrigins(parent, origins) {
    if (origins.length === 0) return;
    const heading = document.createElement("p");
    heading.className = "edge-meta";
    heading.textContent = "SQL origins";
    parent.appendChild(heading);
    const list = document.createElement("ul");
    list.className = "origins";
    for (const origin of origins) {
      const row = document.createElement("li");
      const location = origin.location ? ` at ${formatLocation(origin.location)}` : "";
      row.textContent = `${origin.role}; body hash ${origin.body_hash}${location}`;
      list.appendChild(row);
    }
    parent.appendChild(list);
  }

  function formatLocation(location) {
    return `line ${location.line}, column ${location.column}–line ${location.end_line}, column ${location.end_column}`;
  }
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, Evidence, EvidenceLayer, Vertex, VertexId, VertexKind};

    fn sample_graph() -> Graph {
        sample_graph_with_evidence_order(false)
    }

    fn sample_graph_with_evidence_order(reverse: bool) -> Graph {
        let mut graph = Graph::new();
        let table = VertexId::object("public", "orders");
        let customer = VertexId::object("public", "customers");
        graph.add_vertex(Vertex {
            id: table.clone(),
            kind: VertexKind::Table,
            name: "orders </script><script>alert('owned')</script>".into(),
            schema: "public".into(),
        });
        graph.add_vertex(Vertex {
            id: customer.clone(),
            kind: VertexKind::Table,
            name: "customers".into(),
            schema: "public".into(),
        });
        let mut evidence = vec![
            Evidence {
                layer: EvidenceLayer::Catalog,
                detail: "constraint <script>".into(),
            },
            Evidence {
                layer: EvidenceLayer::BodyParse,
                detail: "body reference".into(),
            },
        ];
        if reverse {
            evidence.reverse();
        }
        graph.add_edge(Edge {
            from: table,
            to: customer,
            kind: schemagraph_core::EdgeKind::References,
            evidence,
        });
        graph
    }

    fn rendered(graph: &Graph) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_html(&mut bytes, graph).expect("HTML writer should accept Vec");
        bytes
    }

    #[test]
    fn untrusted_names_cannot_break_out_of_json_script() {
        let html = String::from_utf8(rendered(&sample_graph())).expect("HTML is UTF-8");
        assert!(html.contains(
            r#"orders \u003c/script\u003e\u003cscript\u003ealert('owned')\u003c/script\u003e"#
        ));
        assert!(html.contains(r#"constraint \u003cscript\u003e"#));
        assert!(!html.contains("orders </script><script>alert('owned')</script>"));
    }

    #[test]
    fn equivalent_renderings_are_byte_deterministic() {
        assert_eq!(
            rendered(&sample_graph_with_evidence_order(false)),
            rendered(&sample_graph_with_evidence_order(true))
        );
    }
}
