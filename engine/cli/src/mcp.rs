//! 고정 그래프를 읽기 전용 MCP stdio 서버로 노출한다.
//!
//! 서버는 stdin의 한 줄 JSON-RPC를 읽고 stdout에 응답 한 줄을 쓴다. 그래프
//! 파일이나 데이터베이스를 요청마다 열지 않으며, 호출자가 보내는 인자는
//! 명시한 범위 안에서만 해석한다.

use std::io::{self, BufRead, Write};

use schemagraph_analysis::{self as analysis, Resolve};
use schemagraph_core::Graph;
#[cfg(test)]
use schemagraph_core::VertexId;
use schemagraph_export as export;
use serde_json::{json, Map, Value};

const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_NAME_BYTES: usize = 4096;
const MAX_DEPTH: u64 = 64;
const MAX_RESULTS: u64 = 10_000;
const MAX_PATHS: u64 = 1024;
const MAX_VISITED: u64 = 100_000;
const MAX_EDGES: u64 = 1_000_000;

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// MCP stdio 서버를 실행한다.
///
/// `graph`는 호출 전에 완성된 읽기 전용 스냅샷이다. `reader`와 `writer`는
/// 테스트에서 메모리 스트림으로 바꿀 수 있도록 일반 스트림으로 받는다.
pub(crate) fn serve<R: BufRead, W: Write>(
    graph: &Graph,
    mut reader: R,
    mut writer: W,
) -> io::Result<()> {
    let mut initialized = false;
    loop {
        match read_bounded_line(&mut reader)? {
            Line::Eof => return Ok(()),
            Line::Oversized => {
                write_response(
                    &mut writer,
                    &error_response(
                        Value::Null,
                        -32600,
                        "Request exceeds the 1 MiB maximum message size",
                    ),
                )?;
            }
            Line::Data(line) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let message = match serde_json::from_slice::<Value>(&line) {
                    Ok(value) => value,
                    Err(error) => {
                        write_response(
                            &mut writer,
                            &error_response(Value::Null, -32700, &format!("Parse error: {error}")),
                        )?;
                        continue;
                    }
                };
                let (response, next_initialized) = handle_message(graph, message, initialized);
                initialized = next_initialized;
                if let Some(response) = response {
                    write_response(&mut writer, &response)?;
                }
            }
        }
    }
}

enum Line {
    Eof,
    Oversized,
    Data(Vec<u8>),
}

/// 한 줄을 제한된 메모리로 읽고, 너무 긴 줄은 다음 요청 경계를 회복한다.
fn read_bounded_line<R: BufRead>(reader: &mut R) -> io::Result<Line> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            if line.is_empty() && !oversized {
                return Ok(Line::Eof);
            }
            return Ok(if oversized {
                Line::Oversized
            } else {
                Line::Data(line)
            });
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |position| position + 1);
        if !oversized {
            if line.len().saturating_add(take) > MAX_REQUEST_BYTES {
                oversized = true;
                line.clear();
            } else {
                line.extend_from_slice(&chunk[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(if oversized {
                Line::Oversized
            } else {
                Line::Data(line)
            });
        }
    }
}

fn write_response(writer: &mut impl Write, response: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn handle_message(graph: &Graph, message: Value, initialized: bool) -> (Option<Value>, bool) {
    let Some(object) = message.as_object() else {
        return (
            Some(error_response(Value::Null, -32600, "Invalid Request")),
            initialized,
        );
    };
    let has_id = object.contains_key("id");
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if has_id && !is_valid_request_id(&id) {
        return (
            Some(error_response(
                Value::Null,
                -32600,
                "Invalid Request: id must be a string or number",
            )),
            initialized,
        );
    }
    let valid_version = object.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
    let method = object.get("method").and_then(Value::as_str);
    let Some(method) = method else {
        return response_or_none(
            has_id,
            error_response(id, -32600, "Invalid Request: method must be a string"),
            initialized,
        );
    };
    if !valid_version {
        return response_or_none(
            has_id,
            error_response(id, -32600, "Invalid Request"),
            initialized,
        );
    }

    if !has_id {
        return (None, handle_notification(method, object, initialized));
    }

    if method == "initialize" {
        return handle_initialize(id, object);
    }
    if method == "ping" {
        if let Err(error) = validate_optional_object_params(object.get("params")) {
            return (Some(error_response(id, -32602, &error)), initialized);
        }
        return (Some(success_response(id, json!({}))), initialized);
    }
    if !initialized {
        return (
            Some(error_response(
                id,
                -32600,
                "Server is not initialized; send initialize and notifications/initialized first",
            )),
            initialized,
        );
    }

    match method {
        "tools/list" => handle_tools_list(id, object),
        "tools/call" => handle_tools_call(graph, id, object),
        _ => (
            Some(error_response(id, -32601, "Method not found")),
            initialized,
        ),
    }
}

fn is_valid_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

fn response_or_none(has_id: bool, response: Value, initialized: bool) -> (Option<Value>, bool) {
    if has_id {
        (Some(response), initialized)
    } else {
        (None, initialized)
    }
}

fn handle_notification(method: &str, _object: &Map<String, Value>, initialized: bool) -> bool {
    match method {
        "notifications/initialized" => true,
        // 취소 알림은 현재 동기 처리 루프가 다음 요청을 읽기 전에 도달할 수
        // 없으므로 상태를 바꾸지 않고 소비한다.
        "notifications/cancelled" => initialized,
        _ => initialized,
    }
}

fn handle_initialize(id: Value, object: &Map<String, Value>) -> (Option<Value>, bool) {
    let params = match required_object_params(object) {
        Ok(params) => params,
        Err(error) => return (Some(error_response(id, -32602, &error)), false),
    };
    let Some(protocol_version) = params.get("protocolVersion").and_then(Value::as_str) else {
        return (
            Some(error_response(
                id,
                -32602,
                "initialize.params.protocolVersion must be a string",
            )),
            false,
        );
    };
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol_version) {
        return (
            Some(error_response(
                id,
                -32602,
                "Unsupported protocol version; supported versions are 2025-11-25, 2025-06-18, 2025-03-26, and 2024-11-05",
            )),
            false,
        );
    }
    if let Some(capabilities) = params.get("capabilities") {
        if !capabilities.is_object() {
            return (
                Some(error_response(
                    id,
                    -32602,
                    "initialize.params.capabilities must be an object",
                )),
                false,
            );
        }
    }
    if let Some(client_info) = params.get("clientInfo") {
        if !client_info.is_object() {
            return (
                Some(error_response(
                    id,
                    -32602,
                    "initialize.params.clientInfo must be an object",
                )),
                false,
            );
        }
    }
    (
        Some(success_response(
            id,
            json!({
                "protocolVersion": protocol_version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "schemagraph", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Read-only schema graph queries over a preloaded graph.",
            }),
        )),
        false,
    )
}

fn handle_tools_list(id: Value, object: &Map<String, Value>) -> (Option<Value>, bool) {
    if let Err(error) = validate_optional_object_params(object.get("params")) {
        return (Some(error_response(id, -32602, &error)), true);
    }
    (
        Some(success_response(id, json!({"tools": tool_definitions()}))),
        true,
    )
}

fn handle_tools_call(
    graph: &Graph,
    id: Value,
    object: &Map<String, Value>,
) -> (Option<Value>, bool) {
    let params = match required_object_params(object) {
        Ok(params) => params,
        Err(error) => return (Some(error_response(id, -32602, &error)), true),
    };
    let Some(tool_name) = params.get("name").and_then(Value::as_str) else {
        return (
            Some(error_response(
                id,
                -32602,
                "tools/call.params.name must be a string",
            )),
            true,
        );
    };
    let arguments = match params.get("arguments") {
        None => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => {
            return (
                Some(error_response(
                    id,
                    -32602,
                    "tools/call.params.arguments must be an object",
                )),
                true,
            )
        }
    };
    let result = match call_tool(graph, tool_name, &arguments) {
        Ok(result) => result,
        Err(error) => return (Some(error_response(id, -32602, &error)), true),
    };
    let is_error = result.is_error;
    (
        Some(success_response(id, tool_result(&result.value, is_error))),
        true,
    )
}

struct ToolResult {
    value: Value,
    is_error: bool,
}

fn call_tool(
    graph: &Graph,
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<ToolResult, String> {
    match name {
        "query" => {
            let name = required_name(arguments, "name")?;
            let depth = bounded_u64(arguments, "depth", 1, MAX_DEPTH)? as u32;
            let max = bounded_u64(arguments, "max", 256, MAX_RESULTS)? as usize;
            let max_visited =
                bounded_u64(arguments, "maxVisited", MAX_VISITED, MAX_VISITED)? as usize;
            let max_examined_edges =
                bounded_u64(arguments, "maxExaminedEdges", MAX_EDGES, MAX_EDGES)? as usize;
            reject_unknown(
                arguments,
                &["name", "depth", "max", "maxVisited", "maxExaminedEdges"],
            )?;
            Ok(resolve_query(
                graph,
                &name,
                depth,
                max,
                max_visited,
                max_examined_edges,
            ))
        }
        "impact" => {
            let name = required_name(arguments, "name")?;
            let max = bounded_u64(arguments, "max", 1024, MAX_RESULTS)? as usize;
            let max_visited =
                bounded_u64(arguments, "maxVisited", MAX_VISITED, MAX_VISITED)? as usize;
            let max_examined_edges =
                bounded_u64(arguments, "maxExaminedEdges", MAX_EDGES, MAX_EDGES)? as usize;
            reject_unknown(
                arguments,
                &["name", "max", "maxVisited", "maxExaminedEdges"],
            )?;
            Ok(resolve_impact(
                graph,
                &name,
                max,
                max_visited,
                max_examined_edges,
            ))
        }
        "explain" => {
            let name = required_name(arguments, "name")?;
            let max = bounded_u64(arguments, "max", 1024, MAX_RESULTS)? as usize;
            reject_unknown(arguments, &["name", "max"])?;
            Ok(resolve_explain(graph, &name, max))
        }
        "diagnostics" => {
            let name = optional_name(arguments, "name")?;
            reject_unknown(arguments, &["name"])?;
            Ok(resolve_diagnostics(graph, name.as_deref()))
        }
        "path" => {
            let from = required_name(arguments, "from")?;
            let to = required_name(arguments, "to")?;
            let options = analysis::paths::SearchOptions {
                max_paths: bounded_u64(arguments, "maxPaths", 32, MAX_PATHS)? as usize,
                max_depth: bounded_u64(arguments, "depth", 32, MAX_DEPTH)? as u32,
                max_visited: bounded_u64(arguments, "maxVisited", 100_000, MAX_VISITED)? as usize,
                max_edges: bounded_u64(arguments, "maxEdges", 1_000_000, MAX_EDGES)? as usize,
                reverse: optional_bool(arguments, "reverse", false)?,
            };
            reject_unknown(
                arguments,
                &[
                    "from",
                    "to",
                    "maxPaths",
                    "depth",
                    "maxVisited",
                    "maxEdges",
                    "reverse",
                ],
            )?;
            Ok(resolve_path(graph, &from, &to, options))
        }
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn resolve_query(
    graph: &Graph,
    name: &str,
    depth: u32,
    max: usize,
    max_visited: usize,
    max_examined_edges: usize,
) -> ToolResult {
    match analysis::resolve(graph, name) {
        Resolve::Found(id) => {
            let budget = analysis::budget::Budget {
                max_visited,
                max_examined_edges,
            };
            let dependents = analysis::budget::walk(graph, &id, depth, max, true, budget, None);
            let dependencies = analysis::budget::walk(graph, &id, depth, max, false, budget, None);
            let mut self_edges: Vec<_> = graph
                .outgoing(&id)
                .iter()
                .filter(|edge| edge.to == id && edge.kind.is_dependency())
                .map(|edge| edge.kind)
                .collect();
            self_edges.sort();
            self_edges.dedup();
            ToolResult {
                value: export::budgeted_query_to_value(
                    graph
                        .vertex(&id)
                        .expect("resolve verified the query subject"),
                    &dependents,
                    &dependencies,
                    depth,
                    &self_edges,
                    graph.limitations(),
                ),
                is_error: false,
            }
        }
        Resolve::NotFound { candidates } => ToolResult {
            value: export::not_found_value(name, &candidates, graph.limitations()),
            is_error: true,
        },
    }
}

fn resolve_impact(
    graph: &Graph,
    name: &str,
    max: usize,
    max_visited: usize,
    max_examined_edges: usize,
) -> ToolResult {
    match analysis::resolve(graph, name) {
        Resolve::Found(id) => {
            let report = analysis::budget::walk(
                graph,
                &id,
                u32::MAX,
                max,
                true,
                analysis::budget::Budget {
                    max_visited,
                    max_examined_edges,
                },
                None,
            );
            ToolResult {
                value: export::budgeted_impact_to_value(
                    graph
                        .vertex(&id)
                        .expect("resolve verified the impact subject"),
                    &report,
                    graph.limitations(),
                ),
                is_error: false,
            }
        }
        Resolve::NotFound { candidates } => ToolResult {
            value: export::not_found_value(name, &candidates, graph.limitations()),
            is_error: true,
        },
    }
}

fn resolve_explain(graph: &Graph, name: &str, max: usize) -> ToolResult {
    match analysis::resolve(graph, name) {
        Resolve::Found(id) => {
            let report = analysis::paths::explain(graph, &id, max)
                .expect("resolve verified the explanation subject exists");
            ToolResult {
                value: export::explain::explanation_value(&report, graph),
                is_error: false,
            }
        }
        Resolve::NotFound { candidates } => ToolResult {
            value: export::not_found_value(name, &candidates, graph.limitations()),
            is_error: true,
        },
    }
}

fn resolve_diagnostics(graph: &Graph, name: Option<&str>) -> ToolResult {
    match name {
        None => ToolResult {
            value: export::diagnostics::report(graph, None),
            is_error: false,
        },
        Some(name) => match analysis::resolve(graph, name) {
            Resolve::Found(id) => ToolResult {
                value: export::diagnostics::report(graph, Some(&id)),
                is_error: false,
            },
            Resolve::NotFound { candidates } => ToolResult {
                value: export::not_found_value(name, &candidates, graph.limitations()),
                is_error: true,
            },
        },
    }
}

fn resolve_path(
    graph: &Graph,
    from: &str,
    to: &str,
    options: analysis::paths::SearchOptions,
) -> ToolResult {
    let from_id = match analysis::resolve(graph, from) {
        Resolve::Found(id) => id,
        Resolve::NotFound { candidates } => {
            return ToolResult {
                value: export::not_found_value(from, &candidates, graph.limitations()),
                is_error: true,
            }
        }
    };
    let to_id = match analysis::resolve(graph, to) {
        Resolve::Found(id) => id,
        Resolve::NotFound { candidates } => {
            return ToolResult {
                value: export::not_found_value(to, &candidates, graph.limitations()),
                is_error: true,
            }
        }
    };
    ToolResult {
        value: export::explain::path_value(&analysis::paths::paths(
            graph, &from_id, &to_id, options, None,
        )),
        is_error: false,
    }
}

fn required_object_params(object: &Map<String, Value>) -> Result<&Map<String, Value>, String> {
    match object.get("params") {
        Some(Value::Object(params)) => Ok(params),
        Some(_) => Err("params must be an object".into()),
        None => Err("params are required".into()),
    }
}

fn validate_optional_object_params(params: Option<&Value>) -> Result<(), String> {
    match params {
        None | Some(Value::Null) | Some(Value::Object(_)) => Ok(()),
        Some(_) => Err("params must be an object".into()),
    }
}

fn required_name(arguments: &Map<String, Value>, key: &str) -> Result<String, String> {
    let Some(name) = arguments.get(key).and_then(Value::as_str) else {
        return Err(format!("argument '{key}' must be a string"));
    };
    validate_name(name)?;
    Ok(name.to_owned())
}

fn optional_name(arguments: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::String(name)) => {
            validate_name(name)?;
            Ok(Some(name.clone()))
        }
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("object names must not be empty".into());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "object names are limited to {MAX_NAME_BYTES} bytes"
        ));
    }
    Ok(())
}

fn bounded_u64(
    arguments: &Map<String, Value>,
    key: &str,
    default: u64,
    maximum: u64,
) -> Result<u64, String> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let Some(value) = value.as_u64() else {
        return Err(format!("argument '{key}' must be a non-negative integer"));
    };
    if value > maximum {
        return Err(format!("argument '{key}' exceeds the maximum of {maximum}"));
    }
    Ok(value)
}

fn optional_bool(arguments: &Map<String, Value>, key: &str, default: bool) -> Result<bool, String> {
    match arguments.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("argument '{key}' must be a boolean")),
    }
}

fn reject_unknown(arguments: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = arguments
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        return Err(format!("unknown argument '{key}'"));
    }
    Ok(())
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": is_error,
    })
}

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_definition(
            "query",
            "List bounded dependency and dependent neighborhoods for one object.",
            json!({
                "type":"object",
                "properties": {
                    "name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "depth":{"type":"integer","minimum":0,"maximum":MAX_DEPTH,"default":1},
                    "max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":256},
                    "maxVisited":{"type":"integer","minimum":0,"maximum":MAX_VISITED,"default":MAX_VISITED},
                    "maxExaminedEdges":{"type":"integer","minimum":0,"maximum":MAX_EDGES,"default":MAX_EDGES}
                },
                "required":["name"],"additionalProperties":false
            }),
        ),
        tool_definition(
            "impact",
            "List objects that depend on one object, with explicit truncation state.",
            json!({
                "type":"object",
                "properties": {
                    "name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":1024},
                    "maxVisited":{"type":"integer","minimum":0,"maximum":MAX_VISITED,"default":MAX_VISITED},
                    "maxExaminedEdges":{"type":"integer","minimum":0,"maximum":MAX_EDGES,"default":MAX_EDGES}
                },
                "required":["name"],"additionalProperties":false
            }),
        ),
        tool_definition(
            "explain",
            "Explain incident edges with evidence, origins, body hashes, locations, and analysis status.",
            json!({
                "type":"object",
                "properties": {
                    "name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":1024}
                },
                "required":["name"],"additionalProperties":false
            }),
        ),
        tool_definition(
            "diagnostics",
            "Report graph-wide or object-level SQL analysis coverage and diagnostics.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES}},
                "additionalProperties":false
            }),
        ),
        tool_definition(
            "path",
            "Find bounded shortest dependency paths between two objects.",
            json!({
                "type":"object",
                "properties": {
                    "from":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "to":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "maxPaths":{"type":"integer","minimum":0,"maximum":MAX_PATHS,"default":32},
                    "depth":{"type":"integer","minimum":0,"maximum":MAX_DEPTH,"default":32},
                    "maxVisited":{"type":"integer","minimum":0,"maximum":MAX_VISITED,"default":100000},
                    "maxEdges":{"type":"integer","minimum":0,"maximum":MAX_EDGES,"default":1000000},
                    "reverse":{"type":"boolean","default":false}
                },
                "required":["from","to"],"additionalProperties":false
            }),
        ),
    ]
}

fn tool_definition(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemagraph_core::{Edge, EdgeKind, Vertex, VertexKind};
    use std::io::Cursor;

    fn fixture() -> Graph {
        let mut graph = Graph::new();
        for name in ["orders", "customers", "report"] {
            graph.add_vertex(Vertex {
                id: VertexId::object("public", name),
                kind: if name == "report" {
                    VertexKind::View
                } else {
                    VertexKind::Table
                },
                name: name.into(),
                schema: "public".into(),
            });
        }
        graph.add_edge(Edge {
            from: VertexId::object("public", "report"),
            to: VertexId::object("public", "orders"),
            kind: EdgeKind::Reads,
            evidence: vec![],
        });
        graph.add_edge(Edge {
            from: VertexId::object("public", "orders"),
            to: VertexId::object("public", "customers"),
            kind: EdgeKind::References,
            evidence: vec![],
        });
        graph
    }

    fn run(input: &str) -> Vec<Value> {
        let mut output = Vec::new();
        serve(&fixture(), Cursor::new(input.as_bytes()), &mut output)
            .expect("stdio server should finish");
        String::from_utf8(output)
            .expect("responses are UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("response JSON"))
            .collect()
    }

    fn handshake() -> String {
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n"
        )
        .into()
    }

    #[test]
    fn handshake_ping_and_tools_list_follow_stdio_lifecycle() {
        let input = format!(
            "{}{}\n{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#
        );
        let responses = run(&input);
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
        assert!(responses[1]["result"].is_object());
        assert_eq!(responses[2]["result"]["tools"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn query_and_missing_target_match_cli_report_shapes() {
        let input = format!(
            "{}{}\n{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"name":"orders"}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"impact","arguments":{"name":"missing"}}}"#
        );
        let responses = run(&input);
        assert_eq!(
            responses[1]["result"]["structuredContent"]["subject"]["id"],
            "public.orders"
        );
        assert_eq!(responses[2]["result"]["isError"], true);
        assert_eq!(responses[2]["result"]["structuredContent"]["found"], false);
    }

    #[test]
    fn query_budget_is_exposed_and_can_mark_the_result_incomplete() {
        let input = format!(
            "{}{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"name":"orders","maxVisited":1,"maxExaminedEdges":100}}}"#
        );
        let responses = run(&input);
        let report = &responses[1]["result"]["structuredContent"];
        assert_eq!(report["complete"], false);
        assert_eq!(report["visited"], 2);
        assert!(report["truncationReasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "visited-limit"));
        assert_eq!(report["traversals"]["dependencies"]["complete"], false);
    }

    #[test]
    fn invalid_arguments_unknown_tools_and_methods_are_json_rpc_errors() {
        let input = format!(
            "{}{}\n{}\n{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"name":"orders","depth":999}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"unknown","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#
        );
        let responses = run(&input);
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert_eq!(responses[2]["error"]["code"], -32602);
        assert_eq!(responses[3]["error"]["code"], -32601);
    }

    #[test]
    fn oversized_input_gets_error_and_notifications_get_no_response() {
        let oversized = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"x\":\"{}\"}}\n",
            "x".repeat(MAX_REQUEST_BYTES)
        );
        let input = format!(
            "{}{}{}\n",
            oversized,
            handshake(),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        );
        let responses = run(&input);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], -32600);
    }

    #[test]
    fn protocol_version_is_negotiated_only_from_supported_versions() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
            "\n"
        );
        let responses = run(input);
        assert_eq!(responses[0]["error"]["code"], -32602);
    }

    #[test]
    fn invalid_request_id_is_rejected() {
        let input = format!(
            "{}{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#
        );
        let responses = run(&input);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[1]["error"]["code"], -32600);
    }
}
