//! OTLP exporter behaviour (TS `otlp-span-exporter.test.ts`,
//! `otlp-export.test.ts`) and the critical-path guard.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;

use pa_types::trace_context::TraceContext;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;

use super::*;

/// One request the fake collector received.
#[derive(Debug, Clone, PartialEq)]
struct Received {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

fn read_request(stream: &mut TcpStream) -> Option<Received> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(':')?;
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_string());
        if key == "content-length" {
            length = value.parse().ok()?;
        }
        headers.push((key, value));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Received {
        path,
        headers,
        body: serde_json::from_slice(&body).ok()?,
    })
}

/// A collector that answers `200` to every request and reports it.
fn answering_collector() -> (String, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let sender = sender.clone();
            std::thread::spawn(move || {
                while let Some(received) = read_request(&mut stream) {
                    let _ = sender.send(received);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
                }
            });
        }
    });
    (endpoint, receiver)
}

/// A collector that reads every request and never answers.
fn hanging_collector() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            if let Some(received) = read_request(&mut stream) {
                let _ = sender.send(received.path);
            }
            held.push(stream);
        }
    });
    (endpoint, receiver)
}

fn span(name: &str, error: Option<&str>) -> FinishedSpan {
    let mut attrs = Map::new();
    attrs.insert("tool.name".to_string(), Value::from("bash"));
    attrs.insert("turn.index".to_string(), Value::from(2));
    attrs.insert("ratio".to_string(), js_number(0.5));
    attrs.insert("tool.aborted".to_string(), Value::from(false));
    attrs.insert("api_key".to_string(), Value::from("sk-secret"));
    attrs.insert("long".to_string(), Value::from("x".repeat(2000)));
    FinishedSpan {
        name: name.to_string(),
        ids: SpanIds {
            context: TraceContext {
                trace_id: 0x0af7_6519_16cd_43dd_8448_eb21_1c80_319c,
                span_id: 0xb7ad_6b71_6920_3331,
                flags: 1,
            },
            parent_span_id: Some(0xc8be_7c82_7031_4442),
        },
        duration_ms: 12.5,
        attrs,
        error: error.map(str::to_string),
    }
}

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn parses_the_header_variable() {
    assert_eq!(
        parse_otlp_headers(Some(
            " Authorization=Bearer%20abc , x-team = core,bad,=empty,dup=1,dup=2,raw=%zz"
        )),
        vec![
            ("Authorization".to_string(), "Bearer abc".to_string()),
            ("x-team".to_string(), "core".to_string()),
            ("dup".to_string(), "2".to_string()),
            ("raw".to_string(), "%zz".to_string()),
        ]
    );
    assert_eq!(parse_otlp_headers(Some("   ")), Vec::new());
    assert_eq!(parse_otlp_headers(None), Vec::new());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one whole-object check per OTLP body"
)]
fn exports_a_full_batch_as_otlp_json_traces_and_delta_metrics() {
    let (endpoint, received) = answering_collector();
    let mut config = OtlpConfig::new(format!("{endpoint}/"));
    config.batch_size = 2;
    config.service_version = Some("1.2.3".to_string());
    config.headers = vec![("x-team".to_string(), "core".to_string())];
    let exporter = OtlpExporter::new(config);
    exporter.export(span("tool.execute", None));
    exporter.export(span("tool.execute", Some("exit 1")));
    let mut requests: Vec<Received> = (0..2)
        .map(|_| received.recv_timeout(WAIT).expect("collector request"))
        .collect();
    requests.sort_by(|left, right| left.path.cmp(&right.path));
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/v1/metrics", "/v1/traces"]
    );
    for request in &requests {
        assert!(request
            .headers
            .contains(&("x-team".to_string(), "core".to_string())));
        assert!(request
            .headers
            .contains(&("content-type".to_string(), "application/json".to_string())));
    }
    let resource = json!({"attributes": [
        {"key": "service.name", "value": {"stringValue": "prime-agent"}},
        {"key": "service.version", "value": {"stringValue": "1.2.3"}},
    ]});
    // Span times are wall-clock: check each span's start/end distance, then
    // compare the rest of the body whole.
    let mut traces = requests[1].body.clone();
    for span in traces["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array_mut()
        .expect("spans")
    {
        let time = |key: &str| -> u128 {
            span[key]
                .as_str()
                .and_then(|text| text.parse().ok())
                .expect("unix nanos")
        };
        assert_eq!(
            time("endTimeUnixNano") - time("startTimeUnixNano"),
            12_500_000
        );
        span["startTimeUnixNano"] = json!("start");
        span["endTimeUnixNano"] = json!("end");
    }
    let attributes = json!([
        {"key": "tool.name", "value": {"stringValue": "bash"}},
        {"key": "turn.index", "value": {"intValue": "2"}},
        {"key": "ratio", "value": {"doubleValue": 0.5}},
        {"key": "tool.aborted", "value": {"boolValue": false}},
        {"key": "long", "value": {"stringValue": "x".repeat(1024)}},
    ]);
    let expected_span = |status: Value| {
        json!({
            "traceId": "0af7651916cd43dd8448eb211c80319c",
            "spanId": "b7ad6b7169203331",
            "parentSpanId": "c8be7c8270314442",
            "name": "tool.execute",
            "kind": 1,
            "startTimeUnixNano": "start",
            "endTimeUnixNano": "end",
            "attributes": attributes.clone(),
            "status": status,
        })
    };
    assert_eq!(
        traces,
        json!({"resourceSpans": [{
            "resource": &resource,
            "scopeSpans": [{"scope": {"name": "prime-agent"}, "spans": [
                expected_span(json!({"code": 1})),
                expected_span(json!({"code": 2, "message": "exit 1"})),
            ]}],
        }]})
    );
    let metrics = &requests[0].body["resourceMetrics"][0];
    assert_eq!(metrics["resource"], resource);
    let names_and_points: Vec<(Value, Value)> = metrics["scopeMetrics"][0]["metrics"]
        .as_array()
        .expect("metrics")
        .iter()
        .map(|metric| {
            let point = &metric["sum"]["dataPoints"][0];
            let value = point
                .get("asInt")
                .or_else(|| point.get("asDouble"))
                .cloned()
                .unwrap_or_default();
            (metric["name"].clone(), value)
        })
        .collect();
    assert_eq!(
        names_and_points,
        vec![
            (json!("prime_agent.span.count"), json!("2")),
            (json!("prime_agent.span.error_count"), json!("1")),
            (json!("prime_agent.span.duration_ms"), json!(25)),
        ]
    );
    assert!(exporter.shutdown(WAIT));
    assert_eq!(
        exporter.stats(),
        OtlpStats {
            queued: 0,
            dropped: 0,
            export_errors: 0,
        }
    );
}

#[test]
fn shutdown_drains_a_partial_batch() {
    let (endpoint, received) = answering_collector();
    let exporter = OtlpExporter::new(OtlpConfig::new(endpoint));
    exporter.export(span("agent.turn", None));
    assert!(exporter.shutdown(WAIT));
    let mut paths: Vec<String> = (0..2)
        .map(|_| received.recv_timeout(WAIT).expect("request").path)
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["/v1/metrics", "/v1/traces"]);
    exporter.export(span("after.stop", None));
    assert_eq!(
        exporter.stats().queued,
        0,
        "a stopped exporter accepts nothing"
    );
}

/// The critical-path guard (AGENTS.md): with a collector that never
/// answers, finishing spans on the caller's thread must never wait on
/// delivery, the queue stays bounded (oldest dropped), and an orderly exit
/// abandons the hanging request at its deadline.
#[test]
fn a_hanging_collector_never_delays_traced_code() {
    let (endpoint, requests) = hanging_collector();
    let mut config = OtlpConfig::new(endpoint);
    config.batch_size = 1;
    config.request_timeout = Duration::from_secs(600);
    let otlp = OtlpExporter::new(config);
    let dir = tempfile::tempdir().expect("tempdir");
    let writer = Arc::new(crate::writer::LogWriter::new(
        crate::log_file::RotatingLog::new(dir.path().join("agent.jsonl")),
    ));
    let layer = crate::TraceLayer::new(Arc::clone(&writer), None, Some(Arc::clone(&otlp)));
    // The first span reaches the worker, whose request then hangs.
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer.clone()), || {
        tracing::info_span!("first.span").in_scope(|| {});
    });
    let first = requests
        .recv_timeout(WAIT)
        .expect("the worker is delivering");
    assert!(first.starts_with("/v1/"), "{first}");

    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            for _ in 0..10_000 {
                tracing::info_span!("traced.work").in_scope(|| {});
            }
        });
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_ok(),
        "finishing spans waited on the hanging collector"
    );
    assert_eq!(
        otlp.stats(),
        OtlpStats {
            queued: 2048,
            dropped: 10_000 - 2048,
            export_errors: 0,
        }
    );
    let started = Instant::now();
    assert!(!otlp.shutdown(Duration::from_millis(200)));
    assert!(started.elapsed() < WAIT);
    assert!(writer.flush(WAIT));
}
