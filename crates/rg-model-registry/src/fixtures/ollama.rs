//! A loopback mock of the Ollama HTTP API for tests. It never runs a language model: each
//! generation is a closed-form function of the bound input values in the prompt, shaped to the
//! JSON schema the client requested. Behaviors simulate runtime faults.

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{json, Map, Value};

use crate::http::LoopbackEndpoint;

pub const FIXTURE_MODEL: &str = "synthetic-fixture-llm:1b";
pub const FIXTURE_DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
pub const FIXTURE_DIGEST_CHANGED: &str =
    "2222222222222222222222222222222222222222222222222222222222222222";

#[derive(Clone, Debug, PartialEq)]
pub enum MockBehavior {
    Normal,
    /// Numeric outputs drift slightly with every generation, as a non-deterministic runtime would.
    VaryingOutput,
    /// Generations return text that is not the requested JSON object.
    MalformedOutput,
    /// Generations add a field the schema does not declare.
    UnknownOutputField,
    /// Generations return a numeric output outside the declared bounds.
    OutOfRangeOutput,
    /// Every numeric output is the given value.
    FixedOutput(f64),
    /// The listing omits digests.
    MalformedListing,
    Redirect,
    Oversized,
    OversizedChunked,
    GarbageHead,
    CloseMidResponse,
    /// Accepts the connection and never answers.
    Stall,
    /// Every generation answers HTTP 500.
    GenerateError,
    /// The connection drops during the given generation (1-based) and every later one.
    CrashAfterGenerations(u64),
}

#[derive(Clone, Debug)]
pub struct MockModel {
    pub name: String,
    pub digest: String,
}

struct MockState {
    behavior: MockBehavior,
    models: Vec<MockModel>,
    requests: Vec<String>,
    /// After this many generations, every listed model reports the given digest.
    digest_flip: Option<(u64, String)>,
}

pub struct MockOllama {
    endpoint: LoopbackEndpoint,
    state: Arc<Mutex<MockState>>,
    generations: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MockOllama {
    pub fn start(behavior: MockBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let endpoint =
            LoopbackEndpoint::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(MockState {
            behavior,
            models: vec![MockModel {
                name: FIXTURE_MODEL.into(),
                digest: FIXTURE_DIGEST.into(),
            }],
            requests: Vec::new(),
            digest_flip: None,
        }));
        let generations = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (state, generations, stop) = (
                Arc::clone(&state),
                Arc::clone(&generations),
                Arc::clone(&stop),
            );
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (state, generations, stop) = (
                                Arc::clone(&state),
                                Arc::clone(&generations),
                                Arc::clone(&stop),
                            );
                            thread::spawn(move || serve(stream, &state, &generations, &stop));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        Self {
            endpoint,
            state,
            generations,
            stop,
            worker: Some(worker),
        }
    }

    pub fn endpoint(&self) -> LoopbackEndpoint {
        self.endpoint
    }

    pub fn set_behavior(&self, behavior: MockBehavior) {
        self.state.lock().unwrap().behavior = behavior;
    }

    pub fn set_models(&self, models: Vec<MockModel>) {
        self.state.lock().unwrap().models = models;
    }

    /// Makes the listing report `digest` for every model once `generations` have been served,
    /// simulating a model tag replaced while a run is in progress.
    pub fn flip_digest_after(&self, generations: u64, digest: &str) {
        self.state.lock().unwrap().digest_flip = Some((generations, digest.into()));
    }

    /// `METHOD PATH` of every request received, in arrival order.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn generations(&self) -> u64 {
        self.generations.load(Ordering::Relaxed)
    }
}

impl Drop for MockOllama {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 || data.len() > 64 * 1024 {
            return None;
        }
        data.extend_from_slice(&buffer[..count]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        body.extend_from_slice(&buffer[..count]);
    }
    Some((method, path, body))
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn respond_chunked(stream: &mut TcpStream, body: &[u8]) {
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    );
    for chunk in body.chunks(97) {
        let _ = stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes());
        let _ = stream.write_all(chunk);
        let _ = stream.write_all(b"\r\n");
    }
    let _ = stream.write_all(b"0\r\n\r\n");
}

fn serve(
    mut stream: TcpStream,
    state: &Mutex<MockState>,
    generations: &AtomicU64,
    stop: &AtomicBool,
) {
    let Some((method, path, body)) = read_request(&mut stream) else {
        return;
    };
    let (behavior, models) = {
        let mut state = state.lock().unwrap();
        state.requests.push(format!("{method} {path}"));
        let mut models = state.models.clone();
        if let Some((after, digest)) = &state.digest_flip {
            if generations.load(Ordering::Relaxed) >= *after {
                for model in &mut models {
                    model.digest.clone_from(digest);
                }
            }
        }
        (state.behavior.clone(), models)
    };
    match behavior {
        MockBehavior::Redirect => {
            let _ = stream.write_all(
                b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://203.0.113.9/api/tags\r\nContent-Length: 0\r\n\r\n",
            );
            return;
        }
        MockBehavior::Oversized => {
            respond(&mut stream, "200 OK", &vec![b' '; 200 * 1024]);
            return;
        }
        MockBehavior::OversizedChunked => {
            respond_chunked(&mut stream, &vec![b' '; 200 * 1024]);
            return;
        }
        MockBehavior::GarbageHead => {
            let _ = stream.write_all(b"SPDY/9 banana\r\n\r\n");
            return;
        }
        MockBehavior::CloseMidResponse => {
            let _ =
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n{\"models\":[");
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        MockBehavior::Stall => {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
            }
            return;
        }
        _ => {}
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/api/version") => respond(&mut stream, "200 OK", br#"{"version":"0.0.0-mock"}"#),
        ("GET", "/api/tags") => {
            let listing = models
                .iter()
                .map(|model| {
                    let mut entry = json!({
                        "name": model.name,
                        "model": model.name,
                        "modified_at": "2026-01-01T00:00:00Z",
                        "size": 1_234_567,
                        "details": {
                            "format": "gguf",
                            "family": "synthetic",
                            "parameter_size": "1B",
                            "quantization_level": "Q4_0"
                        },
                        "unreviewed_future_field": true
                    });
                    if behavior != MockBehavior::MalformedListing {
                        entry["digest"] = json!(model.digest);
                    }
                    entry
                })
                .collect::<Vec<_>>();
            respond_chunked(
                &mut stream,
                serde_json::to_vec(&json!({ "models": listing }))
                    .unwrap()
                    .as_slice(),
            );
        }
        ("POST", "/api/generate") => {
            let count = generations.fetch_add(1, Ordering::Relaxed) + 1;
            if let MockBehavior::CrashAfterGenerations(limit) = behavior {
                if count >= limit {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 400\r\n\r\n{");
                    let _ = stream.shutdown(Shutdown::Both);
                    return;
                }
            }
            if behavior == MockBehavior::GenerateError {
                respond(
                    &mut stream,
                    "500 Internal Server Error",
                    br#"{"error":"mock runtime failure"}"#,
                );
                return;
            }
            let Ok(request) = serde_json::from_slice::<Value>(&body) else {
                respond(&mut stream, "400 Bad Request", br#"{"error":"bad json"}"#);
                return;
            };
            let model = request["model"].as_str().unwrap_or_default();
            if !models.iter().any(|known| known.name == model) {
                respond(
                    &mut stream,
                    "404 Not Found",
                    br#"{"error":"model not found"}"#,
                );
                return;
            }
            let text = generation_text(&request, &behavior, count);
            let response = json!({
                "model": model,
                "created_at": "2026-01-01T00:00:00Z",
                "response": text,
                "done": true,
                "done_reason": "stop",
                "total_duration": 1000,
                "eval_count": 3
            });
            respond(
                &mut stream,
                "200 OK",
                &serde_json::to_vec(&response).unwrap(),
            );
        }
        _ => respond(&mut stream, "404 Not Found", br#"{"error":"not found"}"#),
    }
}

/// Sum of the numeric values in the prompt's `INPUT_JSON:` line.
fn prompt_signal(prompt: &str) -> f64 {
    prompt
        .lines()
        .find_map(|line| line.strip_prefix("INPUT_JSON: "))
        .and_then(|json| serde_json::from_str::<Map<String, Value>>(json).ok())
        .map(|inputs| inputs.values().filter_map(Value::as_f64).sum())
        .unwrap_or(0.0)
}

fn generation_text(request: &Value, behavior: &MockBehavior, count: u64) -> String {
    if *behavior == MockBehavior::MalformedOutput {
        return "The market looks bullish to me.".into();
    }
    let signal = prompt_signal(request["prompt"].as_str().unwrap_or_default());
    let schema = &request["format"];
    let mut output = Map::new();
    let properties = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for name in schema["required"].as_array().cloned().unwrap_or_default() {
        let name = name.as_str().unwrap_or_default().to_owned();
        let property = &properties[&name];
        let value = if let Some(labels) = property["enum"].as_array() {
            let index = if signal > 0.0 { 0 } else { labels.len() - 1 };
            labels[index].clone()
        } else {
            let mut value = signal.tanh();
            if *behavior == MockBehavior::VaryingOutput {
                value += count as f64 * 1e-3;
            }
            if *behavior == MockBehavior::OutOfRangeOutput {
                value = 1.0e6;
            }
            if let MockBehavior::FixedOutput(fixed) = behavior {
                value = *fixed;
            }
            json!(value)
        };
        output.insert(name, value);
    }
    if *behavior == MockBehavior::UnknownOutputField {
        output.insert("position_size".into(), json!(1.0));
    }
    Value::Object(output).to_string()
}
