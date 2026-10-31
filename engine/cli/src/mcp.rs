//! 고정 그래프를 읽기 전용 MCP stdio 서버로 노출한다.
//!
//! 서버는 stdin의 한 줄 JSON-RPC를 읽고 stdout에 응답 한 줄을 쓴다. 그래프
//! 파일이나 데이터베이스를 요청마다 열지 않으며, 호출자가 보내는 인자는
//! 명시한 범위 안에서만 해석한다.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use schemagraph_analysis::{self as analysis, Resolve};
use schemagraph_core::{Graph, Level, VertexId};
use schemagraph_export as export;
use serde_json::{json, Map, Value};

const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_NAME_BYTES: usize = 4096;
const MAX_DEPTH: u64 = 64;
const MAX_RESULTS: u64 = 10_000;
const MAX_PATHS: u64 = 1024;
const MAX_VISITED: u64 = 100_000;
const MAX_EDGES: u64 = 1_000_000;
const MAX_OUTSTANDING_JOBS: usize = 16;

/// `stats`·`cycles`·`diagnostics` 목록의 기본 `max`다.
///
/// 이 도구들은 그래프 전체를 훑어 관측 정점·순환·분석 기록을 모두 싣던 터라, usage를 수집한
/// 대형 그래프에서는 응답 한 줄이 그래프 크기만큼 커졌다. 같은 서버의 `dead`·`unused`·
/// `impact`·`explain` 기본값과 같은 1024로 둔다 — 에이전트가 한 번에 읽을 만한 크기이고,
/// 더 필요하면 `MAX_RESULTS`까지 올려 다시 묻는다. 잘린 목록에는 `truncated`가 붙는다.
const DEFAULT_WHOLE_GRAPH_MAX: u64 = 1024;

/// notFound 응답이 싣는 후보 수의 상한이다.
///
/// 후보는 같은 말단 이름을 가진 정점 전부라, 모든 테이블에 `id` 컬럼이 있는 그래프(리뷰가 잰
/// 사례는 후보 5,000개)에서는 응답이 그래프 크기만큼 커진다. 후보 목록은 다시 고를 이름을
/// 보여 주는 용도라 에이전트가 읽고 고를 수 있는 규모면 충분하다. language-traversal
/// `roots` 상한과 같은 64로 둔다. 잘리면 `candidatesTotal`·`candidatesTruncated`가 붙으므로
/// 소비자는 스키마로 한정한 id로 다시 묻는다.
const MAX_NOT_FOUND_CANDIDATES: usize = 64;

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// MCP 서버가 적재한 읽기 전용 입력 — 그래프와 운영자가 시작할 때 정한 보존 정책이다.
///
/// 보존 정책을 도구 인자로 받지 않는 이유: 에이전트가 넘긴 경로로 서버가
/// 파일을 읽으면 "요청마다 파일을 열지 않는다"는 스냅샷 모델이 깨진다.
pub(crate) struct Snapshot<'a> {
    /// 서버 시작 전에 완성된 그래프.
    pub(crate) graph: &'a Graph,
    /// `dead` 도구가 CLI `dead`와 같은 판정을 내도록 쓰는 보존 정책.
    pub(crate) retention: &'a analysis::RetentionPolicy,
}

/// MCP stdio 서버를 실행한다.
///
/// `snapshot`은 호출 전에 완성된 읽기 전용 입력이다. `reader`와 `writer`는
/// 테스트에서 메모리 스트림으로 바꿀 수 있도록 일반 스트림으로 받는다.
pub(crate) fn serve<R: BufRead + Send + 'static, W: Write + Send>(
    snapshot: &Snapshot,
    reader: R,
    writer: W,
) -> io::Result<()> {
    // 리소스는 스냅샷에서 한 번만 만든다 — 요청 처리 스레드가 그래프를 다시 읽지 않는다.
    let resources = Resources::new(snapshot.graph);
    let writer = Arc::new(Mutex::new(writer));
    let registry = Arc::new(Mutex::new(RequestRegistry::default()));
    let stop_input = Arc::new(AtomicBool::new(false));
    // 입력 프레임도 하나만 대기시켜 큰 요청의 연속 입력을 메모리에 쌓지 않는다.
    let (events_tx, events_rx) = mpsc::sync_channel(1);
    let input_events = events_tx.clone();
    let input_stop = Arc::clone(&stop_input);
    let input = thread::Builder::new()
        .name("mcp-input".into())
        .spawn(move || {
            if catch_unwind(AssertUnwindSafe(|| {
                read_inputs(reader, &input_events, &input_stop)
            }))
            .is_err()
                && input_events
                    .send(Event::Input(Err(io::Error::other(
                        "MCP input reader panicked",
                    ))))
                    .is_err()
            {
                // 조정자가 이미 종료했다면 더 전달할 연결이 없다.
            }
        })?;

    let result = thread::scope(|scope| {
        let (jobs_tx, jobs_rx) = mpsc::sync_channel(MAX_OUTSTANDING_JOBS - 1);
        let worker_writer = Arc::clone(&writer);
        let worker_registry = Arc::clone(&registry);
        let worker = thread::Builder::new()
            .name("mcp-analysis".into())
            .spawn_scoped(scope, move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    worker_loop(snapshot, jobs_rx, worker_writer, worker_registry)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("MCP analysis worker panicked")));
                if events_tx.send(Event::WorkerFinished(result)).is_err() {
                    // 입력/출력 실패로 조정자가 먼저 끝나면 완료 알림도 폐기한다.
                }
            })?;
        let mut result = coordinate(&events_rx, jobs_tx, &registry, &writer, &resources);
        if result.is_err() {
            if let Err(error) = cancel_all(&registry) {
                result = Err(error);
            }
        }
        stop_input.store(true, Ordering::Release);
        // 실패 시 worker의 완료 send가 가득 찬 이벤트 큐에서 join을 막지 않게 한다.
        drop(events_rx);
        if worker.join().is_err() {
            return Err(io::Error::other("MCP analysis worker panicked"));
        }
        result
    });
    stop_input.store(true, Ordering::Release);
    if (result.is_ok() || input.is_finished()) && input.join().is_err() {
        return Err(io::Error::other("MCP input reader panicked"));
    }
    // 치명적 I/O 오류 때 stdin read 자체는 깨울 수 없다. reader가 모든 입력
    // 상태를 소유하므로 안전하게 분리하며 CLI의 즉시 프로세스 종료가 이를 정리한다.
    result
}

enum Event {
    Input(io::Result<Line>),
    WorkerFinished(io::Result<()>),
}

fn read_inputs<R: BufRead>(mut reader: R, events: &SyncSender<Event>, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        let line = read_bounded_line(&mut reader);
        let ended = matches!(&line, Ok(Line::Eof) | Err(_));
        if events.send(Event::Input(line)).is_err() || ended {
            return;
        }
    }
}

fn coordinate<W: Write>(
    events: &Receiver<Event>,
    jobs: SyncSender<Job>,
    registry: &Arc<Mutex<RequestRegistry>>,
    writer: &Arc<Mutex<W>>,
    resources: &Resources,
) -> io::Result<()> {
    let mut jobs = Some(jobs);
    let mut initialized = false;
    loop {
        let event = events
            .recv()
            .map_err(|_| io::Error::other("MCP input and analysis channels closed unexpectedly"))?;
        let line = match event {
            Event::WorkerFinished(result) => return result,
            Event::Input(line) => line?,
        };
        match line {
            Line::Eof => {
                // 입력 종료는 취소가 아니다. 이미 수락한 작업을 drain한 뒤 worker가 종료한다.
                jobs.take();
            }
            Line::Oversized => write_shared(
                writer,
                &error_response(
                    Value::Null,
                    -32600,
                    "Request exceeds the 1 MiB maximum message size",
                ),
            )?,
            Line::Data(line) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let message = match serde_json::from_slice::<Value>(&line) {
                    Ok(message) => message,
                    Err(error) => {
                        write_shared(
                            writer,
                            &error_response(Value::Null, -32700, &format!("Parse error: {error}")),
                        )?;
                        continue;
                    }
                };
                let action = handle_message(message, initialized, registry)?;
                initialized = action.initialized();
                match action {
                    MessageAction::Reply {
                        response: Some(response),
                        ..
                    } => write_shared(writer, &response)?,
                    MessageAction::Reply { response: None, .. } | MessageAction::Cancel { .. } => {}
                    MessageAction::Resource { id, request, .. } => {
                        write_shared(writer, &resources.respond(id, &request))?
                    }
                    MessageAction::Submit { id, call, .. } => {
                        let sender = jobs.as_ref().ok_or_else(|| {
                            io::Error::other("MCP request arrived after input EOF")
                        })?;
                        let error = match enqueue_job(sender, registry, id.clone(), call)? {
                            EnqueueResult::Accepted => None,
                            EnqueueResult::Duplicate => {
                                Some(error_response(id, -32600, "Duplicate active request id"))
                            }
                            EnqueueResult::Busy => Some(error_response(
                                id,
                                -32000,
                                "Server is busy; too many outstanding tool calls",
                            )),
                        };
                        if let Some(error) = error {
                            write_shared(writer, &error)?;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct RequestRegistry {
    requests: HashMap<Value, Arc<AtomicBool>>,
}

struct Job {
    id: Value,
    token: Arc<AtomicBool>,
    call: PreparedToolCall,
}

enum EnqueueResult {
    Accepted,
    Duplicate,
    Busy,
}

enum MessageAction {
    Reply {
        response: Option<Value>,
        initialized: bool,
    },
    Cancel {
        initialized: bool,
    },
    Submit {
        id: Value,
        call: PreparedToolCall,
        initialized: bool,
    },
    /// 사전 계산한 리소스로 조정 스레드가 바로 답한다.
    Resource {
        id: Value,
        request: ResourceRequest,
        initialized: bool,
    },
}

impl MessageAction {
    fn initialized(&self) -> bool {
        match self {
            Self::Reply { initialized, .. }
            | Self::Cancel { initialized }
            | Self::Submit { initialized, .. }
            | Self::Resource { initialized, .. } => *initialized,
        }
    }
}

fn worker_loop<W: Write + Send>(
    snapshot: &Snapshot,
    jobs: Receiver<Job>,
    writer: Arc<Mutex<W>>,
    registry: Arc<Mutex<RequestRegistry>>,
) -> io::Result<()> {
    worker_loop_with(snapshot, jobs, writer, registry, call_tool)
}

// 실행 시작/취소 경합을 시간 지연 없이 검사할 수 있도록 계산 경계만 분리한다.
fn worker_loop_with<W: Write + Send>(
    snapshot: &Snapshot,
    jobs: Receiver<Job>,
    writer: Arc<Mutex<W>>,
    registry: Arc<Mutex<RequestRegistry>>,
    mut evaluate: impl FnMut(&Snapshot, &PreparedToolCall, &AtomicBool) -> ToolResult,
) -> io::Result<()> {
    while let Ok(job) = jobs.recv() {
        if !job.token.load(Ordering::Acquire) {
            let result = evaluate(snapshot, &job.call, &job.token);
            let suppress = finish_job(&registry, &job.id, &job.token)?;
            if !suppress {
                write_shared(
                    &writer,
                    &success_response(job.id, tool_result(&result.value, result.is_error)),
                )?;
            }
        } else {
            finish_job(&registry, &job.id, &job.token)?;
        }
    }
    Ok(())
}

fn enqueue_job(
    sender: &SyncSender<Job>,
    registry: &Arc<Mutex<RequestRegistry>>,
    id: Value,
    call: PreparedToolCall,
) -> io::Result<EnqueueResult> {
    let token = Arc::new(AtomicBool::new(false));
    {
        let mut entries = lock_registry(registry)?;
        if entries.requests.contains_key(&id) {
            return Ok(EnqueueResult::Duplicate);
        }
        if entries.requests.len() >= MAX_OUTSTANDING_JOBS {
            return Ok(EnqueueResult::Busy);
        }
        entries.requests.insert(id.clone(), Arc::clone(&token));
    }
    let job = Job {
        id: id.clone(),
        token: Arc::clone(&token),
        call,
    };
    match sender.try_send(job) {
        Ok(()) => Ok(EnqueueResult::Accepted),
        Err(TrySendError::Full(job)) => {
            remove_job(registry, &job.id, &job.token)?;
            Ok(EnqueueResult::Busy)
        }
        Err(TrySendError::Disconnected(job)) => {
            remove_job(registry, &job.id, &job.token)?;
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "MCP analysis worker stopped before accepting a tool call",
            ))
        }
    }
}

fn finish_job(
    registry: &Arc<Mutex<RequestRegistry>>,
    id: &Value,
    token: &Arc<AtomicBool>,
) -> io::Result<bool> {
    let mut entries = lock_registry(registry)?;
    let suppress = entries
        .requests
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, token) && token.load(Ordering::Acquire));
    if entries
        .requests
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, token))
    {
        entries.requests.remove(id);
    }
    Ok(suppress)
}

fn remove_job(
    registry: &Arc<Mutex<RequestRegistry>>,
    id: &Value,
    token: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut entries = lock_registry(registry)?;
    if entries
        .requests
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, token))
    {
        entries.requests.remove(id);
    }
    Ok(())
}

fn cancel_request(registry: &Arc<Mutex<RequestRegistry>>, id: &Value) -> io::Result<()> {
    let entries = lock_registry(registry)?;
    if let Some(token) = entries.requests.get(id) {
        token.store(true, Ordering::Release);
    }
    Ok(())
}

fn is_active_request(registry: &Arc<Mutex<RequestRegistry>>, id: &Value) -> io::Result<bool> {
    Ok(lock_registry(registry)?.requests.contains_key(id))
}

fn cancel_all(registry: &Arc<Mutex<RequestRegistry>>) -> io::Result<()> {
    let entries = lock_registry(registry)?;
    for token in entries.requests.values() {
        token.store(true, Ordering::Release);
    }
    Ok(())
}

fn lock_registry(
    registry: &Arc<Mutex<RequestRegistry>>,
) -> io::Result<std::sync::MutexGuard<'_, RequestRegistry>> {
    registry
        .lock()
        .map_err(|_| io::Error::other("MCP request registry lock was poisoned"))
}

fn write_shared<W: Write>(writer: &Arc<Mutex<W>>, response: &Value) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("MCP writer lock was poisoned"))?;
    write_response(&mut *writer, response)
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

fn handle_message(
    message: Value,
    initialized: bool,
    registry: &Arc<Mutex<RequestRegistry>>,
) -> io::Result<MessageAction> {
    let Some(object) = message.as_object() else {
        return Ok(MessageAction::Reply {
            response: Some(error_response(Value::Null, -32600, "Invalid Request")),
            initialized,
        });
    };
    let has_id = object.contains_key("id");
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if has_id && !is_valid_request_id(&id) {
        return Ok(MessageAction::Reply {
            response: Some(error_response(
                Value::Null,
                -32600,
                "Invalid Request: id must be a string or number",
            )),
            initialized,
        });
    }
    let valid_version = object.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
    let method = object.get("method").and_then(Value::as_str);
    let Some(method) = method else {
        return Ok(MessageAction::Reply {
            response: has_id
                .then(|| error_response(id, -32600, "Invalid Request: method must be a string")),
            initialized,
        });
    };
    if !valid_version {
        return Ok(MessageAction::Reply {
            response: has_id.then(|| error_response(id, -32600, "Invalid Request")),
            initialized,
        });
    }

    if method == "notifications/cancelled" {
        handle_cancel_notification(object, registry)?;
        return Ok(MessageAction::Cancel { initialized });
    }
    if has_id && is_active_request(registry, &id)? {
        return Ok(MessageAction::Reply {
            response: Some(error_response(id, -32600, "Duplicate active request id")),
            initialized,
        });
    }
    if !has_id {
        return Ok(MessageAction::Reply {
            response: None,
            initialized: method == "notifications/initialized" || initialized,
        });
    }

    if method == "initialize" {
        let (response, initialized) = handle_initialize(id, object);
        return Ok(MessageAction::Reply {
            response,
            initialized,
        });
    }
    if method == "ping" {
        if let Err(error) = validate_optional_object_params(object.get("params")) {
            return Ok(MessageAction::Reply {
                response: Some(error_response(id, -32602, &error)),
                initialized,
            });
        }
        return Ok(MessageAction::Reply {
            response: Some(success_response(id, json!({}))),
            initialized,
        });
    }
    if !initialized {
        return Ok(MessageAction::Reply {
            response: Some(error_response(
                id,
                -32600,
                "Server is not initialized; send initialize and notifications/initialized first",
            )),
            initialized,
        });
    }

    match method {
        "tools/list" => {
            let (response, initialized) = handle_tools_list(id, object);
            Ok(MessageAction::Reply {
                response,
                initialized,
            })
        }
        "tools/call" => Ok(handle_tools_call(id, object)),
        "resources/list" | "resources/read" => Ok(handle_resources(id, method, object)),
        _ => Ok(MessageAction::Reply {
            response: Some(error_response(id, -32601, "Method not found")),
            initialized,
        }),
    }
}

fn is_valid_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

fn handle_cancel_notification(
    object: &Map<String, Value>,
    registry: &Arc<Mutex<RequestRegistry>>,
) -> io::Result<()> {
    // id가 붙은 요청은 취소 알림이 아니며 잘못된 reason도 상태를 바꾸지 않는다.
    if object.contains_key("id") {
        return Ok(());
    }
    let Some(Value::Object(params)) = object.get("params") else {
        return Ok(());
    };
    if params
        .get("reason")
        .is_some_and(|reason| !reason.is_string())
    {
        return Ok(());
    }
    let Some(request_id) = params.get("requestId") else {
        return Ok(());
    };
    if is_valid_request_id(request_id) {
        cancel_request(registry, request_id)?;
    }
    Ok(())
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
                "capabilities": {"tools": {}, "resources": {}},
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

fn handle_tools_call(id: Value, object: &Map<String, Value>) -> MessageAction {
    let params = match required_object_params(object) {
        Ok(params) => params,
        Err(error) => {
            return MessageAction::Reply {
                response: Some(error_response(id, -32602, &error)),
                initialized: true,
            }
        }
    };
    let Some(tool_name) = params.get("name").and_then(Value::as_str) else {
        return MessageAction::Reply {
            response: Some(error_response(
                id,
                -32602,
                "tools/call.params.name must be a string",
            )),
            initialized: true,
        };
    };
    let arguments = match params.get("arguments") {
        None => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => {
            return MessageAction::Reply {
                response: Some(error_response(
                    id,
                    -32602,
                    "tools/call.params.arguments must be an object",
                )),
                initialized: true,
            }
        }
    };
    let call = match prepare_tool_call(tool_name, &arguments) {
        Ok(call) => call,
        Err(error) => {
            return MessageAction::Reply {
                response: Some(argument_error_response(id, error)),
                initialized: true,
            }
        }
    };
    MessageAction::Submit {
        id,
        call,
        initialized: true,
    }
}

/// 스냅샷 요약 리소스의 URI.
const SUMMARY_URI: &str = "schemagraph://graph/summary";
/// 에이전트 출력 계약(`schemagraph skill`) 리소스의 URI.
const SKILL_URI: &str = "schemagraph://skill";

/// 조정 스레드가 답할 리소스 요청이다.
enum ResourceRequest {
    List,
    Read { uri: String },
}

/// 서버 시작 시 한 번 만든 읽기 전용 리소스 — 요약은 그래프가 바뀌지 않으므로 고정이다.
struct Resources {
    summary: String,
}

impl Resources {
    /// 그래프 요약을 결정적 JSON 문자열로 미리 직렬화한다.
    fn new(graph: &Graph) -> Self {
        let summary = serde_json::to_string_pretty(&export::graph_summary_value(graph))
            .unwrap_or_else(|error| {
                // 소비자에게는 파싱 가능한 오류 문서를, 운영자에게는 원인을 남긴다.
                eprintln!("schemagraph serve: graph summary serialization failed: {error}");
                json!({"error": "graph summary is unavailable; see the server log"}).to_string()
            });
        Self { summary }
    }

    /// 목록 또는 한 리소스의 내용을 JSON-RPC 응답으로 만든다.
    fn respond(&self, id: Value, request: &ResourceRequest) -> Value {
        match request {
            ResourceRequest::List => {
                success_response(id, json!({"resources": resource_definitions()}))
            }
            ResourceRequest::Read { uri } => match self.text(uri) {
                Some((mime, text)) => success_response(
                    id,
                    json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}),
                ),
                // MCP 명세의 resource-not-found 코드다.
                None => error_response(
                    id,
                    -32002,
                    &format!("Resource not found: {uri}; call resources/list for available URIs"),
                ),
            },
        }
    }

    /// URI에 해당하는 MIME 타입과 본문이다.
    fn text(&self, uri: &str) -> Option<(&'static str, &str)> {
        match uri {
            SUMMARY_URI => Some(("application/json", self.summary.as_str())),
            SKILL_URI => Some(("text/markdown", include_str!("../SKILL.md"))),
            _ => None,
        }
    }
}

/// `resources/list`가 알리는 리소스 목록이다.
fn resource_definitions() -> Value {
    json!([
        {
            "uri": SUMMARY_URI,
            "name": "graph-summary",
            "description": "Vertex and edge counts by kind, schemas, and collection limitations of the loaded snapshot.",
            "mimeType": "application/json"
        },
        {
            "uri": SKILL_URI,
            "name": "skill",
            "description": "Output contract and usage guide for coding agents (same as `schemagraph skill`).",
            "mimeType": "text/markdown"
        }
    ])
}

/// `resources/list`·`resources/read` 요청을 검증해 조정 스레드 작업으로 바꾼다.
fn handle_resources(id: Value, method: &str, object: &Map<String, Value>) -> MessageAction {
    let request = if method == "resources/list" {
        validate_optional_object_params(object.get("params")).map(|()| ResourceRequest::List)
    } else {
        required_object_params(object).and_then(|params| {
            params
                .get("uri")
                .and_then(Value::as_str)
                .map(|uri| ResourceRequest::Read {
                    uri: uri.to_owned(),
                })
                .ok_or_else(|| "resources/read.params.uri must be a string".to_owned())
        })
    };
    match request {
        Ok(request) => MessageAction::Resource {
            id,
            request,
            initialized: true,
        },
        Err(error) => MessageAction::Reply {
            response: Some(error_response(id, -32602, &error)),
            initialized: true,
        },
    }
}

struct ToolResult {
    value: Value,
    is_error: bool,
}

enum PreparedToolCall {
    Query {
        name: String,
        depth: u32,
        max: usize,
        max_visited: usize,
        max_examined_edges: usize,
    },
    Impact {
        name: String,
        max: usize,
        max_visited: usize,
        max_examined_edges: usize,
    },
    Explain {
        name: String,
        max: usize,
    },
    Diagnostics {
        name: Option<String>,
        max: usize,
    },
    Path {
        from: String,
        to: String,
        options: analysis::paths::SearchOptions,
    },
    Search {
        pattern: String,
        kind: Option<schemagraph_core::VertexKind>,
        detail: export::search::SearchDetail,
        max: usize,
    },
    Dead {
        max: usize,
    },
    Cycles {
        level: Level,
        max: usize,
    },
    Lint {
        max: usize,
    },
    Stats {
        max: usize,
    },
    Unused {
        max: usize,
    },
}

/// 도구 인자 검증 실패. 어느 채널로 알릴지가 종류마다 다르다.
///
/// 범위 밖 숫자(`max`·`depth`·예산 인자)는 설계 원칙 4("사용자가 준 인자 값이 허용 범위를
/// 벗어나면 MCP isError로 거부한다")와 MCP 사양 2025-11-25("value out of range" 같은 입력 검증
/// 오류는 `isError` 도구 실행 오류)를 따른다. 에이전트는 이 결과를 읽고 값을 고쳐 다시 부를
/// 수 있다. 같은 검사기를 쓰는 모든 도구가 같은 채널을 쓴다. 모르는 도구, 형식이 틀린 값,
/// 모르는 인자, 이름 오류는 기존대로 JSON-RPC -32602다.
#[derive(Debug)]
enum ArgumentError {
    /// JSON-RPC -32602로 답한다.
    Invalid(String),
    /// 선언한 범위(0부터 최대값까지)를 벗어난 정수. `isError` 도구 결과로 답한다.
    OutOfRange(String),
}

/// 기존 인자 검사기(`String` 오류)는 모두 -32602 채널이다.
impl From<String> for ArgumentError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

/// 인자 검증 실패를 JSON-RPC 응답으로 만든다. 범위 오류 결과의 본문도 다른 도구 결과처럼
/// `content[0].text`의 압축 JSON(`{"error": …}`)이라, 텍스트를 JSON으로 읽는 소비자가 깨지지
/// 않는다.
fn argument_error_response(id: Value, error: ArgumentError) -> Value {
    match error {
        ArgumentError::Invalid(message) => error_response(id, -32602, &message),
        ArgumentError::OutOfRange(message) => {
            success_response(id, tool_result(&json!({"error": message}), true))
        }
    }
}

fn prepare_tool_call(
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<PreparedToolCall, ArgumentError> {
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
            Ok(PreparedToolCall::Query {
                name,
                depth,
                max,
                max_visited,
                max_examined_edges,
            })
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
            Ok(PreparedToolCall::Impact {
                name,
                max,
                max_visited,
                max_examined_edges,
            })
        }
        "explain" => {
            let name = required_name(arguments, "name")?;
            let max = bounded_u64(arguments, "max", 1024, MAX_RESULTS)? as usize;
            reject_unknown(arguments, &["name", "max"])?;
            Ok(PreparedToolCall::Explain { name, max })
        }
        "diagnostics" => {
            let name = optional_name(arguments, "name")?;
            let max = whole_graph_max(arguments)?;
            reject_unknown(arguments, &["name", "max"])?;
            Ok(PreparedToolCall::Diagnostics { name, max })
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
            Ok(PreparedToolCall::Path { from, to, options })
        }
        "search" => prepare_search(arguments),
        "dead" => {
            let max = bounded_u64(arguments, "max", 1024, MAX_RESULTS)? as usize;
            reject_unknown(arguments, &["max"])?;
            Ok(PreparedToolCall::Dead { max })
        }
        "cycles" => {
            let level = match optional_name(arguments, "level")? {
                None => Level::Object,
                Some(label) => crate::parse_level_label(&label).ok_or_else(|| {
                    format!("unknown level '{label}'; use schema, object, column, or member")
                })?,
            };
            let max = whole_graph_max(arguments)?;
            reject_unknown(arguments, &["level", "max"])?;
            Ok(PreparedToolCall::Cycles { level, max })
        }
        "lint" => {
            let max = bounded_u64(arguments, "max", 256, MAX_RESULTS)? as usize;
            reject_unknown(arguments, &["max"])?;
            Ok(PreparedToolCall::Lint { max })
        }
        "stats" => {
            let max = whole_graph_max(arguments)?;
            reject_unknown(arguments, &["max"])?;
            Ok(PreparedToolCall::Stats { max })
        }
        "unused" => {
            let max = bounded_u64(arguments, "max", 1024, MAX_RESULTS)? as usize;
            reject_unknown(arguments, &["max"])?;
            Ok(PreparedToolCall::Unused { max })
        }
        _ => Err(format!("Unknown tool: {name}").into()),
    }
}

/// `search` 인자를 CLI와 같은 규칙으로 검증한다.
fn prepare_search(arguments: &Map<String, Value>) -> Result<PreparedToolCall, ArgumentError> {
    let pattern = required_name(arguments, "pattern")?;
    let kind = optional_name(arguments, "kind")?;
    let detail = match optional_name(arguments, "detail")?.as_deref() {
        None => export::search::SearchDetail::Names,
        Some(label) => export::search::SearchDetail::parse(label)
            .ok_or_else(|| format!("unknown detail '{label}'; use names or summary"))?,
    };
    let max = bounded_u64(arguments, "max", 100, MAX_RESULTS)? as usize;
    reject_unknown(arguments, &["pattern", "kind", "detail", "max"])?;
    let kind = crate::parse_search_filters(&pattern, kind.as_deref())
        .map_err(|error| error.to_string())?;
    Ok(PreparedToolCall::Search {
        pattern,
        kind,
        detail,
        max,
    })
}

fn call_tool(snapshot: &Snapshot, call: &PreparedToolCall, cancel: &AtomicBool) -> ToolResult {
    let graph = snapshot.graph;
    match call {
        PreparedToolCall::Query {
            name,
            depth,
            max,
            max_visited,
            max_examined_edges,
        } => resolve_query(
            graph,
            name,
            *depth,
            *max,
            *max_visited,
            *max_examined_edges,
            cancel,
        ),
        PreparedToolCall::Impact {
            name,
            max,
            max_visited,
            max_examined_edges,
        } => resolve_impact(graph, name, *max, *max_visited, *max_examined_edges, cancel),
        PreparedToolCall::Explain { name, max } => resolve_explain(graph, name, *max, cancel),
        PreparedToolCall::Diagnostics { name, max } => {
            resolve_diagnostics(graph, name.as_deref(), *max, cancel)
        }
        PreparedToolCall::Path { from, to, options } => {
            resolve_path(graph, from, to, *options, cancel)
        }
        PreparedToolCall::Search {
            pattern,
            kind,
            detail,
            max,
        } => whole_graph(cancel, || {
            let query = analysis::search::SearchQuery {
                pattern,
                kind: *kind,
                max: *max,
                count_neighbors: *detail == export::search::SearchDetail::Summary,
            };
            let report = analysis::search::search(graph, query);
            export::search::to_value(&report, pattern, *kind, *detail, graph.limitations())
        }),
        PreparedToolCall::Dead { max } => whole_graph(cancel, || {
            export::dead_to_value(&analysis::dead_with_policy(graph, *max, snapshot.retention))
        }),
        PreparedToolCall::Cycles { level, max } => whole_graph(cancel, || {
            export::cycles_to_value_with_max(&analysis::cycles(graph, *level), *max)
        }),
        PreparedToolCall::Lint { max } => whole_graph(cancel, || {
            export::lint::to_value(&analysis::schema_lint::lint(graph, *max))
        }),
        PreparedToolCall::Stats { max } => {
            whole_graph(cancel, || export::stats_to_value_with_max(graph, *max))
        }
        PreparedToolCall::Unused { max } => whole_graph(cancel, || {
            export::unused::to_value(&analysis::unused::unused(graph, *max), graph.limitations())
        }),
    }
}

/// 중간 취소 지점이 없는 전체 그래프 질의를 실행한다.
///
/// 계산 앞뒤에서만 취소를 확인한다 — `diagnostics`와 같은 경계로, 취소된
/// 호출은 부분 결과를 완전한 답처럼 내지 않는다.
fn whole_graph(cancel: &AtomicBool, compute: impl FnOnce() -> Value) -> ToolResult {
    if is_cancelled(cancel) {
        return cancelled_result();
    }
    let value = compute();
    if is_cancelled(cancel) {
        return cancelled_result();
    }
    ToolResult {
        value,
        is_error: false,
    }
}

fn resolve_query(
    graph: &Graph,
    name: &str,
    depth: u32,
    max: usize,
    max_visited: usize,
    max_examined_edges: usize,
    cancel: &AtomicBool,
) -> ToolResult {
    match analysis::resolve(graph, name) {
        Resolve::Found(id) => {
            let budget = analysis::budget::Budget {
                max_visited,
                max_examined_edges,
            };
            let dependents =
                analysis::budget::walk(graph, &id, depth, max, true, budget, Some(cancel));
            let dependencies =
                analysis::budget::walk(graph, &id, depth, max, false, budget, Some(cancel));
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
        Resolve::NotFound { candidates } => not_found_result(graph, name, &candidates),
    }
}

fn resolve_impact(
    graph: &Graph,
    name: &str,
    max: usize,
    max_visited: usize,
    max_examined_edges: usize,
    cancel: &AtomicBool,
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
                Some(cancel),
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
        Resolve::NotFound { candidates } => not_found_result(graph, name, &candidates),
    }
}

fn resolve_explain(graph: &Graph, name: &str, max: usize, cancel: &AtomicBool) -> ToolResult {
    match analysis::resolve(graph, name) {
        Resolve::Found(id) => {
            if is_cancelled(cancel) {
                return cancelled_result();
            }
            let report = analysis::paths::explain(graph, &id, max)
                .expect("resolve verified the explanation subject exists");
            if is_cancelled(cancel) {
                return cancelled_result();
            }
            ToolResult {
                value: export::explain::explanation_value(&report, graph),
                is_error: false,
            }
        }
        Resolve::NotFound { candidates } => not_found_result(graph, name, &candidates),
    }
}

/// 진단 보고 — `objects`만 `max`개로 자르고 `summary`는 전체 기록으로 센다.
///
/// 이름을 준 보고는 기록이 하나뿐이라 `max`가 0일 때만 잘린다. 그래도 같은 인자를 같은
/// 뜻으로 적용한다 — 이름 유무에 따라 `max`를 조용히 무시하지 않는다.
fn resolve_diagnostics(
    graph: &Graph,
    name: Option<&str>,
    max: usize,
    cancel: &AtomicBool,
) -> ToolResult {
    if is_cancelled(cancel) {
        return cancelled_result();
    }
    let result = match name {
        None => ToolResult {
            value: export::diagnostics::report_with_max(graph, None, max),
            is_error: false,
        },
        Some(name) => match analysis::resolve(graph, name) {
            Resolve::Found(id) => ToolResult {
                value: export::diagnostics::report_with_max(graph, Some(&id), max),
                is_error: false,
            },
            Resolve::NotFound { candidates } => not_found_result(graph, name, &candidates),
        },
    };
    if is_cancelled(cancel) {
        cancelled_result()
    } else {
        result
    }
}

fn resolve_path(
    graph: &Graph,
    from: &str,
    to: &str,
    options: analysis::paths::SearchOptions,
    cancel: &AtomicBool,
) -> ToolResult {
    let from_id = match analysis::resolve(graph, from) {
        Resolve::Found(id) => id,
        Resolve::NotFound { candidates } => return not_found_result(graph, from, &candidates),
    };
    let to_id = match analysis::resolve(graph, to) {
        Resolve::Found(id) => id,
        Resolve::NotFound { candidates } => return not_found_result(graph, to, &candidates),
    };
    ToolResult {
        value: export::explain::path_value(&analysis::paths::paths(
            graph,
            &from_id,
            &to_id,
            options,
            Some(cancel),
        )),
        is_error: false,
    }
}

fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Acquire)
}

fn cancelled_result() -> ToolResult {
    ToolResult {
        value: Value::Null,
        is_error: false,
    }
}

/// 이름을 해석하지 못한 호출의 notFound 결과 — 후보는 [`MAX_NOT_FOUND_CANDIDATES`]개까지 싣는다.
///
/// 모든 도구가 같은 상한을 쓰도록 한 곳에서 만든다. 잘리지 않은 결과는 CLI notFound와 같다.
fn not_found_result(graph: &Graph, name: &str, candidates: &[VertexId]) -> ToolResult {
    ToolResult {
        value: export::not_found_value_with_max(
            name,
            candidates,
            graph.limitations(),
            MAX_NOT_FOUND_CANDIDATES,
        ),
        is_error: true,
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

/// 0부터 `maximum`까지의 정수 인자를 읽는다. 없으면 `default`다.
///
/// 범위 밖 정수(음수 포함)는 조용히 깎지 않고 [`ArgumentError::OutOfRange`]로 거부한다(설계
/// 원칙 4). 정수가 아닌 값은 형식 오류([`ArgumentError::Invalid`])다.
fn bounded_u64(
    arguments: &Map<String, Value>,
    key: &str,
    default: u64,
    maximum: u64,
) -> Result<u64, ArgumentError> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    match value.as_u64() {
        Some(value) if value <= maximum => Ok(value),
        Some(_) => Err(out_of_range(
            key,
            &format!("exceeds the maximum of {maximum}"),
            maximum,
        )),
        None if value.is_i64() => Err(out_of_range(key, "is negative", maximum)),
        None => Err(format!("argument '{key}' must be a non-negative integer").into()),
    }
}

/// 범위 밖 숫자 인자 오류. 받을 수 있는 범위를 함께 알려 에이전트가 고쳐 다시 부르게 한다.
fn out_of_range(key: &str, problem: &str, maximum: u64) -> ArgumentError {
    ArgumentError::OutOfRange(format!(
        "argument '{key}' {problem}; pass an integer from 0 to {maximum}"
    ))
}

/// `stats`·`cycles`·`diagnostics`의 `max` — 범위 밖 값은 다른 도구의 숫자 인자처럼 isError다.
fn whole_graph_max(arguments: &Map<String, Value>) -> Result<usize, ArgumentError> {
    Ok(bounded_u64(arguments, "max", DEFAULT_WHOLE_GRAPH_MAX, MAX_RESULTS)? as usize)
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

/// 도구 결과를 MCP `CallToolResult`로 감싼다 — 보고 JSON은 `content[0].text`에 한 번만 싣는다.
///
/// 서버가 받는 모든 프로토콜 버전(2024-11-05~2025-11-25)과 클라이언트가 공통으로 읽는
/// 채널이 `content`다. `structuredContent`는 2025-06-18에 생긴 선택 필드이고 도구에
/// `outputSchema`가 없어 요구되지 않는다. 같은 값을 두 번(그것도 pretty 텍스트로) 실으면
/// 응답이 두 배 넘게 커질 뿐 더 전하는 사실이 없다. `Value`의 `Display`는 압축 JSON을
/// 쓰고, serde_json의 `preserve_order`가 꺼져 있어 맵 키가 정렬된다 — CLI JSON과 같은
/// 값이 결정적인 바이트로 나간다. 문자열 키와 유한한 숫자만 담는 `Value`라 직렬화는
/// 실패하지 않는다.
fn tool_result(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": value.to_string()}],
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
            "Report graph-wide or object-level SQL analysis coverage and diagnostics; `objects` lists at most `max` records and adds `truncated` when cut, while `summary` counts every record.",
            json!({
                "type":"object",
                "properties": {
                    "name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":DEFAULT_WHOLE_GRAPH_MAX}
                },
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
        tool_definition(
            "search",
            "Find objects by case-insensitive id/name substring or `*`/`?` glob; start with detail=names and request summary only for candidates.",
            json!({
                "type":"object",
                "properties": {
                    "pattern":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "kind":{"type":"string","enum":export::VERTEX_KIND_LABELS},
                    "detail":{"type":"string","enum":["names","summary"],"default":"names"},
                    "max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":100}
                },
                "required":["pattern"],"additionalProperties":false
            }),
        ),
        tool_definition(
            "dead",
            "Report views/routines unreachable from retention roots set when the server started; a candidate is a graph fact, not a deletion verdict.",
            json!({
                "type":"object",
                "properties":{"max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":1024}},
                "additionalProperties":false
            }),
        ),
        tool_definition(
            "cycles",
            "Report dependency cycles and self-loops at a graph level; lists at most `max` cycles and adds `truncated` and `total` when cut.",
            json!({
                "type":"object",
                "properties": {
                    "level":{"type":"string","enum":["schema","object","column","member"],"default":"object"},
                    "max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":DEFAULT_WHOLE_GRAPH_MAX}
                },
                "additionalProperties":false
            }),
        ),
        tool_definition(
            "lint",
            "Report confirmed and unverified catalog facts such as FK index prefixes and unresolved references.",
            json!({
                "type":"object",
                "properties":{"max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":256}},
                "additionalProperties":false
            }),
        ),
        tool_definition(
            "stats",
            "List collected usage counters with their `since` windows and coverage totals; lists at most `max` entries and adds `truncated` when cut, while `totals` counts every entry.",
            json!({
                "type":"object",
                "properties":{"max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":DEFAULT_WHOLE_GRAPH_MAX}},
                "additionalProperties":false
            }),
        ),
        tool_definition(
            "unused",
            "Report tables and indexes with zero observed reads since their statistics window began, with facts that weaken a removal reading; objects without counters are counted as unobserved, not unused.",
            json!({
                "type":"object",
                "properties":{"max":{"type":"integer","minimum":0,"maximum":MAX_RESULTS,"default":1024}},
                "additionalProperties":false
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
    use schemagraph_core::{
        AnalysisState, Edge, EdgeKind, ObjectAnalysis, Usage, Vertex, VertexKind,
    };
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

    /// 보존 정책이 없는 테스트 스냅샷 — CLI `dead`의 기본값과 같다.
    fn snapshot(graph: &Graph) -> Snapshot<'_> {
        static NO_RETENTION: std::sync::OnceLock<analysis::RetentionPolicy> =
            std::sync::OnceLock::new();
        Snapshot {
            graph,
            retention: NO_RETENTION.get_or_init(analysis::RetentionPolicy::default),
        }
    }

    fn run(input: &str) -> Vec<Value> {
        run_on(&fixture(), input)
    }

    /// 주어진 그래프로 stdio 세션 하나를 돌리고 응답을 출력 순서대로 돌려준다.
    fn run_on(graph: &Graph, input: &str) -> Vec<Value> {
        let mut output = Vec::new();
        serve(
            &snapshot(graph),
            Cursor::new(input.as_bytes().to_vec()),
            &mut output,
        )
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
        let names: Vec<&str> = responses[2]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "query",
                "impact",
                "explain",
                "diagnostics",
                "path",
                "search",
                "dead",
                "cycles",
                "lint",
                "stats",
                "unused"
            ]
        );
        assert!(responses[0]["result"]["capabilities"]["resources"].is_object());
    }

    /// 한 번의 세션에서 도구 호출 결과만 요청 id 순으로 모아 돌려준다.
    fn call(tool_calls: &[&str]) -> Vec<Value> {
        call_on(&fixture(), tool_calls)
    }

    /// `call`과 같되 주어진 그래프를 적재한 서버에 보낸다.
    fn call_on(graph: &Graph, tool_calls: &[&str]) -> Vec<Value> {
        let mut input = handshake();
        for (index, params) in tool_calls.iter().enumerate() {
            input.push_str(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{params}}}"#,
                index + 2
            ));
            input.push('\n');
        }
        let mut responses = run_on(graph, &input);
        responses.remove(0);
        // 인자 오류는 조정 스레드가, 도구 결과는 워커가 쓰므로 도착 순서가 요청 순서와 다를 수 있다.
        responses.sort_by_key(|response| response["id"].as_u64());
        responses
    }

    /// 도구 결과의 유일한 본문인 `content[0].text`를 JSON으로 읽는다.
    ///
    /// 같은 값을 `structuredContent`로 한 번 더 싣지 않는지, 텍스트가 키 정렬된 압축
    /// 직렬화와 바이트까지 같은지도 함께 확인한다 — 모든 도구 테스트가 이 계약을 지나간다.
    fn tool_value(response: &Value) -> Value {
        let result = &response["result"];
        assert!(
            result.get("structuredContent").is_none(),
            "tool results carry their JSON once: {result}"
        );
        let content = result["content"]
            .as_array()
            .expect("tool content is an array");
        assert_eq!(content.len(), 1, "one content block: {result}");
        assert_eq!(content[0]["type"], "text");
        let text = content[0]["text"]
            .as_str()
            .expect("text content is a string");
        let value: Value = serde_json::from_str(text).expect("text content is JSON");
        assert_eq!(
            text,
            value.to_string(),
            "text is compact JSON with sorted keys"
        );
        value
    }

    #[test]
    fn tool_results_carry_the_report_once_as_compact_text() {
        let responses = call(&[
            r#"{"name":"stats"}"#,
            r#"{"name":"query","arguments":{"name":"missing"}}"#,
        ]);
        for (response, is_error) in responses.iter().zip([false, true]) {
            let keys: Vec<&str> = response["result"]
                .as_object()
                .expect("tool result is an object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, ["content", "isError"]);
            assert_eq!(response["result"]["isError"], is_error);
        }
        assert_eq!(
            tool_value(&responses[0]),
            export::stats_to_value(&fixture())
        );
        assert_eq!(tool_value(&responses[1])["found"], false);
    }

    #[test]
    fn search_discloses_names_then_summary_and_rejects_unknown_kind() {
        let responses = call(&[
            r#"{"name":"search","arguments":{"pattern":"OR"}}"#,
            r#"{"name":"search","arguments":{"pattern":"public.*","kind":"VIEW","detail":"summary"}}"#,
            r#"{"name":"search","arguments":{"pattern":"x","kind":"tabel"}}"#,
        ]);
        let names = &tool_value(&responses[0]);
        assert_eq!(names["matches"], json!(["public.orders", "public.report"]));
        assert_eq!(
            (names["total"].as_u64(), names["truncated"].as_bool()),
            (Some(2), Some(false))
        );
        assert!(names.get("kind").is_none());
        let summary = &tool_value(&responses[1]);
        assert_eq!(summary["kind"], "view");
        assert_eq!(summary["matches"][0]["id"], "public.report");
        assert_eq!(summary["matches"][0]["dependencies"], 1);
        assert_eq!(responses[2]["error"]["code"], -32602);
        assert!(responses[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown vertex kind"));
    }

    #[test]
    fn whole_graph_tools_match_cli_exports() {
        let graph = fixture();
        let responses = call(&[
            r#"{"name":"dead","arguments":{}}"#,
            r#"{"name":"cycles","arguments":{"level":"column"}}"#,
            r#"{"name":"lint","arguments":{"max":3}}"#,
            r#"{"name":"stats"}"#,
            r#"{"name":"cycles","arguments":{"level":"table"}}"#,
            r#"{"name":"unused","arguments":{}}"#,
        ]);
        let content = |index: usize| tool_value(&responses[index]);
        assert_eq!(
            content(0),
            export::dead_to_value(&analysis::dead(&graph, 1024))
        );
        assert_eq!(
            content(1),
            export::cycles_to_value(&analysis::cycles(&graph, Level::Member))
        );
        assert_eq!(
            content(2),
            export::lint::to_value(&analysis::schema_lint::lint(&graph, 3))
        );
        assert_eq!(content(3), export::stats_to_value(&graph));
        assert_eq!(
            content(5),
            export::unused::to_value(&analysis::unused::unused(&graph, 1024), graph.limitations())
        );
        assert_eq!(responses[4]["error"]["code"], -32602);
    }

    /// 사용 통계 3개, 순환 2개(두 정점 순환과 자기 루프), 분석 기록 3개를 가진 그래프.
    ///
    /// 작은 `max`로 목록을 자르는 경우와 기본 상한에서 CLI export와 같은 경우를 함께 본다.
    fn bounded_fixture() -> Graph {
        let mut graph = Graph::new();
        let states = [
            AnalysisState::Complete,
            AnalysisState::Partial,
            AnalysisState::Unsupported,
        ];
        for (index, state) in states.into_iter().enumerate() {
            let name = format!("t{index}");
            let id = VertexId::object("app", &name);
            graph.add_vertex(Vertex {
                id: id.clone(),
                kind: VertexKind::Table,
                name,
                schema: "app".into(),
            });
            let usage = Usage {
                reads: index as u64,
                ..Usage::default()
            };
            graph.set_usage(id.clone(), usage);
            let analysis = ObjectAnalysis {
                state,
                scope: "body".into(),
                body_hash: None,
                diagnostics: vec![],
                source: None,
            };
            graph.set_analysis(id, analysis);
        }
        for (from, to) in [("t0", "t1"), ("t1", "t0"), ("t2", "t2")] {
            graph.add_edge(Edge {
                from: VertexId::object("app", from),
                to: VertexId::object("app", to),
                kind: EdgeKind::References,
                evidence: vec![],
            });
        }
        graph
    }

    #[test]
    fn whole_graph_lists_are_cut_at_max_and_marked_only_when_cut() {
        let graph = bounded_fixture();
        let responses = call_on(
            &graph,
            &[
                r#"{"name":"stats","arguments":{"max":2}}"#,
                r#"{"name":"cycles","arguments":{"max":1}}"#,
                r#"{"name":"diagnostics","arguments":{"max":2}}"#,
                r#"{"name":"stats"}"#,
                r#"{"name":"cycles"}"#,
                r#"{"name":"diagnostics"}"#,
            ],
        );
        let stats = tool_value(&responses[0]);
        assert_eq!(stats["stats"].as_array().unwrap().len(), 2);
        assert_eq!(stats["truncated"], true);
        assert_eq!(stats["totals"]["observed"], 3);
        let cycles = tool_value(&responses[1]);
        assert_eq!(cycles["cycles"].as_array().unwrap().len(), 1);
        assert_eq!(
            (&cycles["truncated"], &cycles["total"]),
            (&json!(true), &json!(2))
        );
        let diagnostics = tool_value(&responses[2]);
        assert_eq!(diagnostics["objects"].as_array().unwrap().len(), 2);
        assert_eq!(diagnostics["truncated"], true);
        assert_eq!(
            diagnostics["summary"],
            json!({"complete": 1, "partial": 1, "unsupported": 1, "total": 3})
        );
        // 기본 상한(1024) 안이면 잘림 키 없이 CLI export와 같은 값이다.
        assert_eq!(tool_value(&responses[3]), export::stats_to_value(&graph));
        let whole_cycles = tool_value(&responses[4]);
        assert_eq!(whole_cycles["cycles"].as_array().unwrap().len(), 2);
        assert_eq!(
            whole_cycles,
            export::cycles_to_value(&analysis::cycles(&graph, Level::Object))
        );
        assert_eq!(
            tool_value(&responses[5]),
            export::diagnostics::report(&graph, None)
        );
    }

    /// 말단 이름이 `id`인 테이블 65개와 `dup`인 테이블 2개를 가진 그래프.
    fn ambiguous_fixture() -> Graph {
        let mut graph = Graph::new();
        let names = (0..65)
            .map(|index| (format!("s{index:02}"), "id"))
            .chain([("a".to_owned(), "dup"), ("b".to_owned(), "dup")]);
        for (schema, name) in names {
            graph.add_vertex(Vertex {
                id: VertexId::object(&schema, name),
                kind: VertexKind::Table,
                name: name.into(),
                schema,
            });
        }
        graph
    }

    #[test]
    fn not_found_lists_at_most_64_candidates_with_their_total() {
        let graph = ambiguous_fixture();
        let responses = call_on(
            &graph,
            &[
                r#"{"name":"query","arguments":{"name":"id"}}"#,
                r#"{"name":"impact","arguments":{"name":"id"}}"#,
                r#"{"name":"explain","arguments":{"name":"id"}}"#,
                r#"{"name":"diagnostics","arguments":{"name":"id"}}"#,
                r#"{"name":"path","arguments":{"from":"id","to":"a.dup"}}"#,
                r#"{"name":"path","arguments":{"from":"a.dup","to":"id"}}"#,
                r#"{"name":"query","arguments":{"name":"dup"}}"#,
            ],
        );
        for response in &responses[..6] {
            assert_eq!(response["result"]["isError"], true, "{response}");
            let value = tool_value(response);
            let candidates = value["candidates"].as_array().unwrap();
            // 기존 후보 순서(정점 id 순)의 앞 64개를 남긴다.
            assert_eq!(candidates.len(), 64);
            assert_eq!(
                (&candidates[0], &candidates[63]),
                (&json!("s00.id"), &json!("s63.id"))
            );
            assert_eq!(value["candidatesTotal"], 65);
            assert_eq!(value["candidatesTruncated"], true);
        }
        // 상한 안의 notFound는 CLI와 같은 모양이다 — 잘림 키가 없다.
        let duplicates = [VertexId::object("a", "dup"), VertexId::object("b", "dup")];
        assert_eq!(
            tool_value(&responses[6]),
            export::not_found_value("dup", &duplicates, graph.limitations())
        );
    }

    /// 범위 밖 정수는 isError 도구 결과(원칙 4), 정수가 아닌 값은 JSON-RPC -32602다.
    #[test]
    fn bounded_whole_graph_tools_reject_max_outside_the_declared_range() {
        let responses = call(&[
            r#"{"name":"stats","arguments":{"max":10001}}"#,
            r#"{"name":"cycles","arguments":{"max":-1}}"#,
            r#"{"name":"diagnostics","arguments":{"max":"10"}}"#,
            r#"{"name":"diagnostics","arguments":{"name":"orders","max":1.5}}"#,
            r#"{"name":"stats","arguments":{"max":10000}}"#,
        ]);
        for (response, problem) in responses[..2]
            .iter()
            .zip(["exceeds the maximum of 10000", "is negative"])
        {
            assert_eq!(response["result"]["isError"], true, "{response}");
            let message = tool_value(response)["error"].as_str().unwrap().to_owned();
            assert_eq!(
                message,
                format!("argument 'max' {problem}; pass an integer from 0 to 10000")
            );
        }
        for response in &responses[2..4] {
            assert_eq!(response["error"]["code"], -32602, "{response}");
            assert!(response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("must be a non-negative integer"));
        }
        assert_eq!(
            tool_value(&responses[4]),
            export::stats_to_value(&fixture())
        );
    }

    /// 숫자 인자는 도구와 무관하게 같은 검사기를 지나 같은 채널(isError)로 거부된다. 새
    /// `max`만 isError로 내면 같은 서버의 `dead`·`unused` `max`와 채널이 갈린다. 숫자 인자
    /// 키(`max`·`depth`·`maxVisited`·`maxExaminedEdges`·`maxPaths`·`maxEdges`)마다 사례를 두어
    /// 한 키만 다른 검사 경로(-32602 등)로 새도 이 테스트가 잡게 한다.
    #[test]
    fn out_of_range_numbers_are_tool_errors_for_every_tool() {
        let cases = [
            (
                r#"{"name":"query","arguments":{"name":"orders","depth":65}}"#,
                "depth",
                64,
            ),
            (
                r#"{"name":"query","arguments":{"name":"orders","max":10001}}"#,
                "max",
                10_000,
            ),
            (
                r#"{"name":"impact","arguments":{"name":"orders","maxVisited":100001}}"#,
                "maxVisited",
                100_000,
            ),
            (
                r#"{"name":"query","arguments":{"name":"orders","maxExaminedEdges":1000001}}"#,
                "maxExaminedEdges",
                1_000_000,
            ),
            (
                r#"{"name":"impact","arguments":{"name":"orders","maxExaminedEdges":-1}}"#,
                "maxExaminedEdges",
                1_000_000,
            ),
            (
                r#"{"name":"explain","arguments":{"name":"orders","max":10001}}"#,
                "max",
                10_000,
            ),
            (
                r#"{"name":"path","arguments":{"from":"orders","to":"customers","maxPaths":1025}}"#,
                "maxPaths",
                1024,
            ),
            (
                r#"{"name":"path","arguments":{"from":"orders","to":"customers","maxEdges":1000001}}"#,
                "maxEdges",
                1_000_000,
            ),
            (
                r#"{"name":"search","arguments":{"pattern":"o","max":10001}}"#,
                "max",
                10_000,
            ),
            (
                r#"{"name":"dead","arguments":{"max":10001}}"#,
                "max",
                10_000,
            ),
            (
                r#"{"name":"lint","arguments":{"max":10001}}"#,
                "max",
                10_000,
            ),
            (
                r#"{"name":"unused","arguments":{"max":10001}}"#,
                "max",
                10_000,
            ),
            (
                r#"{"name":"query","arguments":{"name":"orders","depth":-1}}"#,
                "depth",
                64,
            ),
        ];
        let calls: Vec<&str> = cases.iter().map(|(call, _, _)| *call).collect();
        let responses = call(&calls);
        for (response, (call, key, maximum)) in responses.iter().zip(cases) {
            assert_eq!(response["result"]["isError"], true, "{call}: {response}");
            let message = tool_value(response)["error"].as_str().unwrap().to_owned();
            assert!(
                message.starts_with(&format!("argument '{key}' "))
                    && message.ends_with(&format!("; pass an integer from 0 to {maximum}")),
                "{call}: {message}"
            );
        }
    }

    #[test]
    fn bounded_whole_graph_tools_advertise_max() {
        let input = format!(
            "{}{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#
        );
        let responses = run(&input);
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        for name in ["diagnostics", "cycles", "stats"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(
                tool["inputSchema"]["properties"]["max"],
                json!({"type": "integer", "minimum": 0, "maximum": 10000, "default": 1024}),
                "{name}"
            );
        }
    }

    #[test]
    fn resources_list_and_read_summary_and_skill() {
        let input = format!(
            "{}{}\n{}\n{}\n{}\n{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"schemagraph://graph/summary"}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"schemagraph://skill"}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"file:///etc/passwd"}}"#,
            r#"{"jsonrpc":"2.0","id":6,"method":"resources/read","params":{}}"#
        );
        let responses = run(&input);
        let uris: Vec<&str> = responses[1]["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|resource| resource["uri"].as_str().unwrap())
            .collect();
        assert_eq!(uris, [SUMMARY_URI, SKILL_URI]);
        let summary = &responses[2]["result"]["contents"][0];
        assert_eq!(summary["mimeType"], "application/json");
        let summary: Value = serde_json::from_str(summary["text"].as_str().unwrap()).unwrap();
        assert_eq!(summary, export::graph_summary_value(&fixture()));
        assert_eq!(summary["vertices"], json!({"table": 2, "view": 1}));
        assert_eq!(summary["edges"], json!({"reads": 1, "references": 1}));
        assert_eq!(
            responses[3]["result"]["contents"][0]["text"],
            include_str!("../SKILL.md")
        );
        // 임의 URI로 파일을 읽지 않는다 — 알려진 두 리소스만 답한다.
        assert_eq!(responses[4]["error"]["code"], -32002);
        assert_eq!(responses[5]["error"]["code"], -32602);
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
        let report = &tool_value(&responses[1]);
        assert_eq!(report["subject"]["id"], "public.orders");
        assert_eq!(report["format"], "schemagraph-query");
        assert_eq!(report["version"], 1);
        // 직접 이웃의 via는 subject다 — CLI와 같은 직렬화기를 쓴다.
        assert_eq!(report["dependents"][0]["id"], "public.report");
        assert_eq!(report["dependents"][0]["via"], "public.orders");
        assert_eq!(report["dependencies"][0]["id"], "public.customers");
        assert_eq!(report["dependencies"][0]["via"], "public.orders");
        assert_eq!(responses[2]["result"]["isError"], true);
        assert_eq!(tool_value(&responses[2])["found"], false);
        // notFound는 impact 보고가 아니라 별도 모양이다 — 형식 머리를 싣지 않는다.
        assert!(tool_value(&responses[2]).get("format").is_none());
    }

    #[test]
    fn impact_report_carries_format_header_and_via() {
        let input = format!(
            "{}{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"impact","arguments":{"name":"customers"}}}"#
        );
        let responses = run(&input);
        let report = &tool_value(&responses[1]);
        assert_eq!(report["format"], "schemagraph-impact");
        assert_eq!(report["version"], 1);
        // report -> orders -> customers: orders는 subject에서, report는 orders에서 온다.
        let impacted: Vec<(&str, &str)> = report["impacted"]
            .as_array()
            .expect("impacted is an array")
            .iter()
            .map(|n| (n["id"].as_str().unwrap(), n["via"].as_str().unwrap()))
            .collect();
        assert_eq!(
            impacted,
            [
                ("public.orders", "public.customers"),
                ("public.report", "public.orders")
            ]
        );
    }

    #[test]
    fn query_budget_is_exposed_and_can_mark_the_result_incomplete() {
        let input = format!(
            "{}{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"name":"orders","maxVisited":1,"maxExaminedEdges":100}}}"#
        );
        let responses = run(&input);
        let report = &tool_value(&responses[1]);
        assert_eq!(report["complete"], false);
        assert_eq!(report["visited"], 2);
        assert!(report["truncationReasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "visited-limit"));
        assert_eq!(report["traversals"]["dependencies"]["complete"], false);
    }

    /// 범위 밖 숫자는 isError 결과다(`out_of_range_numbers_are_tool_errors_for_every_tool`).
    /// 형식이 틀린 인자·모르는 도구·메서드는 JSON-RPC 오류로 남는다.
    #[test]
    fn invalid_arguments_unknown_tools_and_methods_are_json_rpc_errors() {
        let input = format!(
            "{}{}\n{}\n{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"name":"orders","depth":"deep"}}}"#,
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

    fn query_call() -> PreparedToolCall {
        let arguments = json!({"name": "orders"});
        prepare_tool_call("query", arguments.as_object().unwrap()).expect("query arguments")
    }

    #[test]
    fn cancellation_preserves_id_types_and_ignores_unknown_ids() {
        let registry = Arc::new(Mutex::new(RequestRegistry::default()));
        let token = Arc::new(AtomicBool::new(false));
        registry
            .lock()
            .unwrap()
            .requests
            .insert(json!(7), Arc::clone(&token));

        cancel_request(&registry, &json!("7")).expect("string cancellation should be accepted");
        assert!(!token.load(Ordering::Acquire));
        cancel_request(&registry, &json!(8)).expect("unknown cancellation should be accepted");
        assert!(!token.load(Ordering::Acquire));
        cancel_request(&registry, &json!(7)).expect("number cancellation should be accepted");
        assert!(token.load(Ordering::Acquire));

        let malformed =
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":true}});
        handle_message(malformed, true, &registry).expect("malformed cancellation is a no-op");
        let missing = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{}});
        handle_message(missing, true, &registry).expect("missing cancellation id is a no-op");
    }

    #[test]
    fn queued_cancellation_suppresses_one_job_and_later_job_runs() {
        let graph = fixture();
        let registry = Arc::new(Mutex::new(RequestRegistry::default()));
        let (sender, receiver) = mpsc::sync_channel(2);
        let writer = Arc::new(Mutex::new(Vec::new()));

        assert!(matches!(
            enqueue_job(&sender, &registry, json!(1), query_call()),
            Ok(EnqueueResult::Accepted)
        ));
        assert!(matches!(
            enqueue_job(&sender, &registry, json!(2), query_call()),
            Ok(EnqueueResult::Accepted)
        ));
        cancel_request(&registry, &json!(1)).expect("queued request should be cancellable");
        drop(sender);
        worker_loop(
            &snapshot(&graph),
            receiver,
            Arc::clone(&writer),
            Arc::clone(&registry),
        )
        .expect("worker should finish without an error");

        let output = writer.lock().unwrap().clone();
        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 2);
        assert!(registry.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn cancellation_flag_reaches_query_and_path_traversals() {
        let graph = fixture();
        let query = query_call();
        let flag = AtomicBool::new(true);
        let result = call_tool(&snapshot(&graph), &query, &flag);
        assert_eq!(result.value["complete"], false);
        assert!(result.value["truncationReasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "cancelled"));

        let path_arguments = json!({"from":"report","to":"customers"});
        let path =
            prepare_tool_call("path", path_arguments.as_object().unwrap()).expect("path arguments");
        let path_result = call_tool(&snapshot(&graph), &path, &flag);
        assert!(path_result.value["truncationReasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "cancelled"));
    }

    #[test]
    fn duplicate_and_full_queue_leave_original_registry_entry_intact() {
        let registry = Arc::new(Mutex::new(RequestRegistry::default()));
        let (sender, _receiver) = mpsc::sync_channel(0);
        assert!(matches!(
            enqueue_job(&sender, &registry, json!(1), query_call()),
            Ok(EnqueueResult::Busy)
        ));
        assert!(registry.lock().unwrap().requests.is_empty());

        let (sender, _receiver) = mpsc::sync_channel(1);
        assert!(matches!(
            enqueue_job(&sender, &registry, json!(1), query_call()),
            Ok(EnqueueResult::Accepted)
        ));
        assert!(matches!(
            enqueue_job(&sender, &registry, json!(1), query_call()),
            Ok(EnqueueResult::Duplicate)
        ));
        assert_eq!(registry.lock().unwrap().requests.len(), 1);
    }

    #[test]
    fn active_cancellation_reaches_the_worker_without_affecting_next_request() {
        let graph = fixture();
        let registry = Arc::new(Mutex::new(RequestRegistry::default()));
        let writer = Arc::new(Mutex::new(Vec::new()));
        let (jobs, receiver) = mpsc::sync_channel(2);
        let (started, ready) = mpsc::channel();
        let (proceed, gate) = mpsc::channel();
        enqueue_job(&jobs, &registry, json!(2), query_call()).unwrap();
        enqueue_job(&jobs, &registry, json!(3), query_call()).unwrap();
        drop(jobs);
        thread::scope(|scope| {
            let output = Arc::clone(&writer);
            let active = Arc::clone(&registry);
            let worker = scope.spawn(move || {
                let mut first = true;
                worker_loop_with(
                    &snapshot(&graph),
                    receiver,
                    output,
                    active,
                    |graph, call, cancel| {
                        if first {
                            first = false;
                            assert!(!cancel.load(Ordering::Acquire));
                            started.send(()).unwrap();
                            gate.recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap();
                            assert!(cancel.load(Ordering::Acquire));
                            let result = call_tool(graph, call, cancel);
                            assert_eq!(result.value["complete"], false);
                            result
                        } else {
                            assert!(!cancel.load(Ordering::Acquire));
                            call_tool(graph, call, cancel)
                        }
                    },
                )
            });
            ready
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let notification = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}});
            handle_message(notification, true, &registry).unwrap();
            proceed.send(()).unwrap();
            worker.join().unwrap().unwrap();
        });
        let bytes = writer.lock().unwrap();
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["id"], 3);
        assert_eq!(tool_value(&response)["complete"], true);
        assert!(registry.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn full_queue_accepts_controls_and_rejects_duplicate_control_ids() {
        let registry = Arc::new(Mutex::new(RequestRegistry::default()));
        let (jobs, _receiver) = mpsc::sync_channel(MAX_OUTSTANDING_JOBS);
        for id in 0..MAX_OUTSTANDING_JOBS {
            assert!(matches!(
                enqueue_job(&jobs, &registry, json!(id), query_call()),
                Ok(EnqueueResult::Accepted)
            ));
        }
        assert!(matches!(
            enqueue_job(&jobs, &registry, json!(99), query_call()),
            Ok(EnqueueResult::Busy)
        ));
        for method in ["ping", "tools/list", "initialize"] {
            let duplicate = json!({"jsonrpc":"2.0","id":0,"method":method});
            match handle_message(duplicate, true, &registry).unwrap() {
                MessageAction::Reply {
                    response: Some(response),
                    ..
                } => assert_eq!(response["error"]["code"], -32600),
                _ => panic!("duplicate active control id must be rejected"),
            }
        }
        for malformed in [
            json!({"jsonrpc":"2.0","id":123,"method":"notifications/cancelled","params":{"requestId":0}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":0,"reason":42}}),
        ] {
            handle_message(malformed, true, &registry).unwrap();
            assert!(!registry.lock().unwrap().requests[&json!(0)].load(Ordering::Acquire));
        }
        handle_message(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":0}}),
            true,
            &registry,
        )
        .unwrap();
        let token = registry.lock().unwrap().requests[&json!(0)].clone();
        assert!(finish_job(&registry, &json!(0), &token).unwrap());
        cancel_request(&registry, &json!(0)).unwrap();
        assert!(registry
            .lock()
            .unwrap()
            .requests
            .values()
            .all(|token| !token.load(Ordering::Acquire)));
    }

    #[test]
    fn worker_write_failure_returns_while_input_is_still_open() {
        struct OpenInput {
            data: Cursor<Vec<u8>>,
            until_closed: Receiver<()>,
        }
        impl io::Read for OpenInput {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let count = io::Read::read(&mut self.data, bytes)?;
                if count == 0 {
                    // 이 receiver가 열려 있는 동안 실제 stdin처럼 read가 대기한다.
                    match self.until_closed.recv() {
                        Ok(()) | Err(_) => return Ok(0),
                    }
                }
                Ok(count)
            }
        }
        #[derive(Default)]
        struct FailingToolOutput(Vec<u8>);
        impl Write for FailingToolOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                let response: Value = serde_json::from_slice(&self.0).unwrap();
                self.0.clear();
                if response["id"] == 2 {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "test client closed stdout",
                    ))
                } else {
                    Ok(())
                }
            }
        }
        let input = format!(
            "{}{}\n",
            handshake(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"name":"orders"}}}"#
        );
        let (hold_open, until_closed) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let server = thread::spawn(move || {
            let input = OpenInput {
                data: Cursor::new(input.into_bytes()),
                until_closed,
            };
            let outcome = serve(
                &snapshot(&fixture()),
                io::BufReader::new(input),
                FailingToolOutput::default(),
            );
            finished
                .send(outcome.map_err(|error| error.kind()))
                .unwrap();
        });
        let outcome = result.recv_timeout(std::time::Duration::from_secs(5));
        // 실패한 예전 구현도 테스트가 끝날 때 입력을 풀어 thread가 남지 않게 한다.
        drop(hold_open);
        server.join().unwrap();
        assert_eq!(
            outcome.expect("writer failure must not await stdin EOF"),
            Err(io::ErrorKind::BrokenPipe)
        );
    }
}
