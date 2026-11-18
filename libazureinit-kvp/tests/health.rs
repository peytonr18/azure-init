// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "wireserver")]

use std::collections::HashMap;
use std::error::Error as _;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libazureinit_kvp::{
    DiagnosticWriter, HealthProtocol, HealthWriteError, HealthWriter,
    HealthWriterBuildError, KvpPool, KvpPoolStore, Outcome, PoolMode,
    ProvisioningReport, ReportPpsType, ReportResult, WireserverClient,
    WireserverConfig, WireserverError, PROVISIONING_REPORT_KEY,
};
use reqwest::StatusCode;
use rstest::rstest;
use serde::Deserialize;
use serde_json::{json, Value};
use tempfile::TempDir;

const AGENT: &str = "afterburn/test";
const VM_ID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
    headers: &'static str,
}

impl Reply {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            headers: "",
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

struct Server {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_requests = requests.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            for reply in replies {
                let mut stream = loop {
                    if worker_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock =>
                        {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = match read_request(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) => continue,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut
                                | io::ErrorKind::WouldBlock
                                | io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => panic!("mock request failed: {error}"),
                };
                worker_requests.lock().unwrap().push(request);
                thread::sleep(reply.delay);
                let result = write!(
                    stream,
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.headers,
                    reply.body
                );
                if let Err(error) = result {
                    assert!(
                        matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::ConnectionAborted
                        ),
                        "mock response failed: {error}"
                    );
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn config(&self) -> WireserverConfig {
        WireserverConfig {
            endpoint: format!("http://{}/provisioning/health", self.address),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            total_retry_timeout: Duration::from_secs(3),
            retry_interval: Duration::from_millis(1),
            max_attempts: Some(3),
            xml_fallback: false,
            use_system_proxy: false,
        }
    }

    fn client(&self) -> WireserverClient {
        WireserverClient::new(AGENT, self.config()).unwrap()
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_for_requests(&self, count: usize) {
        let started = Instant::now();
        while self.requests.lock().unwrap().len() < count {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "expected {count} mock requests"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            assert!(
                result.is_ok() || thread::panicking(),
                "mock WireServer worker panicked"
            );
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Ok(None);
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
        {
            break end + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut lines = head.lines();
    let mut start = lines.next().unwrap().split_whitespace();
    let method = start.next().unwrap().to_owned();
    let path = start.next().unwrap().to_owned();
    let headers: HashMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |value| value.parse().unwrap());
    while bytes.len() < header_end + length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Ok(None);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8(
            bytes[header_end..header_end + length].to_vec(),
        )
        .unwrap(),
    }))
}

fn report(result: ReportResult) -> ProvisioningReport {
    match result {
        ReportResult::Success => {
            ProvisioningReport::success(AGENT, VM_ID, ReportPpsType::None)
        }
        ReportResult::Error => ProvisioningReport::failure(
            AGENT,
            VM_ID,
            "bad <config> & \"quoted\" | details\nnext line",
            ReportPpsType::None,
        )
        .with_documentation_url("https://example.test/help")
        .with_extra("context", "preserved"),
        ReportResult::InProgress => {
            ProvisioningReport::in_progress(AGENT, VM_ID, ReportPpsType::None)
        }
    }
}

fn goalstate(incarnation: &str) -> String {
    format!(
        "<GoalState><Incarnation>{incarnation}</Incarnation><Container>\
         <ContainerId>container&amp;id</ContainerId><RoleInstanceList>\
         <RoleInstance><InstanceId>role&lt;id</InstanceId></RoleInstance>\
         </RoleInstanceList></Container></GoalState>"
    )
}

fn store(dir: &TempDir) -> KvpPoolStore {
    KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe).unwrap()
}

#[rstest]
#[case(ReportResult::Success)]
#[case(ReportResult::Error)]
#[case(ReportResult::InProgress)]
fn json_reports_match_the_existing_contract(#[case] result: ReportResult) {
    let server = Server::new(vec![Reply::new(200, "")]);
    let report = report(result);
    let delivery = server.client().write_report(&report).unwrap();
    assert_eq!(delivery.protocol, HealthProtocol::Json);
    assert_eq!(delivery.json_attempts, 1);
    assert_eq!(delivery.xml_attempts, 0);
    assert_eq!(delivery.fallback_status, None);
    let requests = server.requests();
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/provisioning/health");
    assert_eq!(request.headers["user-agent"], AGENT);
    assert_eq!(request.headers["x-ms-guest-agent-name"], AGENT);
    assert_eq!(request.headers["content-type"], "application/json");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let expected = match result {
        ReportResult::Success => json!({ "state": "Ready" }),
        ReportResult::Error => json!({
            "state": "NotReady",
            "details": {
                "subStatus": "ProvisioningFailed",
                "description": report.encode(),
            },
        }),
        ReportResult::InProgress => json!({
            "state": "NotReady",
            "details": {
                "subStatus": "Provisioning",
                "description": format!("Provisioning is still in progress for vm_id={VM_ID}."),
            },
        }),
    };
    assert_eq!(body, expected);
}

#[rstest]
#[case(200)]
#[case(201)]
#[case(202)]
#[case(204)]
fn accepts_successful_acknowledgements(#[case] status: u16) {
    let server = Server::new(vec![Reply::new(status, "")]);
    assert_eq!(server.client().report_success().unwrap().json_attempts, 1);
}

#[rstest]
#[case(400)]
#[case(404)]
#[case(410)]
#[case(429)]
#[case(500)]
#[case(503)]
fn retries_json_without_rebuilding_the_report(#[case] status: u16) {
    let server = Server::new(vec![Reply::new(status, ""), Reply::new(200, "")]);
    let delivery = server
        .client()
        .report_failure(&report(ReportResult::Error))
        .unwrap();
    assert_eq!(delivery.json_attempts, 2);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
}

#[derive(Debug, Deserialize)]
struct XmlEnvelope {
    #[serde(rename = "GoalStateIncarnation")]
    incarnation: String,
    #[serde(rename = "Container")]
    container: XmlContainer,
}

#[derive(Debug, Deserialize)]
struct XmlContainer {
    #[serde(rename = "ContainerId")]
    id: String,
    #[serde(rename = "RoleInstanceList")]
    instances: XmlInstances,
}

#[derive(Debug, Deserialize)]
struct XmlInstances {
    #[serde(rename = "Role")]
    role: XmlRole,
}

#[derive(Debug, Deserialize)]
struct XmlRole {
    #[serde(rename = "InstanceId")]
    id: String,
    #[serde(rename = "Health")]
    health: XmlHealth,
}

#[derive(Debug, Deserialize)]
struct XmlHealth {
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "Details")]
    details: Option<XmlDetails>,
}

#[derive(Debug, Deserialize)]
struct XmlDetails {
    #[serde(rename = "SubStatus")]
    substatus: String,
    #[serde(rename = "Description")]
    description: String,
}

#[rstest]
#[case(404)]
#[case(405)]
#[case(501)]
fn xml_fallback_preserves_state_and_context(
    #[case] status: u16,
    #[values(
        ReportResult::Success,
        ReportResult::Error,
        ReportResult::InProgress
    )]
    result: ReportResult,
) {
    let server = Server::new(vec![
        Reply::new(status, ""),
        Reply::new(200, goalstate("42")),
        Reply::new(200, ""),
    ]);
    let report = report(result);
    let config = WireserverConfig {
        xml_fallback: true,
        max_attempts: Some(1),
        ..server.config()
    };
    let delivery = WireserverClient::new(AGENT, config)
        .unwrap()
        .write_report(&report)
        .unwrap();
    assert_eq!(delivery.protocol, HealthProtocol::Xml);
    assert_eq!(delivery.json_attempts, 1);
    assert_eq!(delivery.xml_attempts, 1);
    assert_eq!(
        delivery.fallback_status,
        Some(StatusCode::from_u16(status).unwrap())
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].path, "/machine/?comp=goalstate");
    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path, "/machine/?comp=health");
    assert_eq!(requests[2].headers["x-ms-agent-name"], AGENT);
    assert_eq!(requests[2].headers["x-ms-version"], "2012-11-30");
    assert_eq!(
        requests[2].headers["content-type"],
        "text/xml; charset=utf-8"
    );
    let xml: XmlEnvelope = serde_xml_rs::from_str(&requests[2].body).unwrap();
    assert_eq!(xml.incarnation, "42");
    assert_eq!(xml.container.id, "container&id");
    assert_eq!(xml.container.instances.role.id, "role<id");
    let health = xml.container.instances.role.health;
    match result {
        ReportResult::Success => {
            assert_eq!(health.state, "Ready");
            assert!(health.details.is_none());
        }
        ReportResult::Error => {
            assert_eq!(health.state, "NotReady");
            let details = health.details.unwrap();
            assert_eq!(details.substatus, "ProvisioningFailed");
            assert_eq!(details.description, report.encode());
        }
        ReportResult::InProgress => {
            assert_eq!(health.state, "NotReady");
            let details = health.details.unwrap();
            assert_eq!(details.substatus, "Provisioning");
            assert_eq!(
                details.description,
                format!("Provisioning is still in progress for vm_id={VM_ID}.")
            );
        }
    }
}

#[rstest]
#[case(302)]
#[case(400)]
#[case(401)]
#[case(403)]
#[case(410)]
#[case(429)]
#[case(500)]
#[case(503)]
fn other_failures_do_not_select_xml(#[case] status: u16) {
    let mut reply = Reply::new(status, "");
    reply.headers = "Location: /machine/?comp=health\r\n";
    let server = Server::new(vec![reply]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            xml_fallback: true,
            max_attempts: Some(1),
            ..server.config()
        },
    )
    .unwrap();
    let error = client.report_success().unwrap_err();
    assert!(!matches!(error, WireserverError::Fallback { .. }));
    assert!(error.source().is_some() || status == 302);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn opt_in_is_required_for_xml() {
    let server = Server::new(vec![Reply::new(404, "")]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            max_attempts: Some(1),
            ..server.config()
        },
    )
    .unwrap();
    assert!(matches!(
        client.report_success(),
        Err(WireserverError::AttemptsExhausted { attempts: 1, .. })
    ));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn json_success_does_not_fetch_legacy_context() {
    let server = Server::new(vec![Reply::new(200, "")]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            xml_fallback: true,
            ..server.config()
        },
    )
    .unwrap();
    assert_eq!(
        client.report_success().unwrap().protocol,
        HealthProtocol::Json
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn xml_retries_refresh_goal_state() {
    let server = Server::new(vec![
        Reply::new(404, ""),
        Reply::new(200, goalstate("1")),
        Reply::new(410, ""),
        Reply::new(200, goalstate("2")),
        Reply::new(200, ""),
    ]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            xml_fallback: true,
            max_attempts: Some(2),
            ..server.config()
        },
    )
    .unwrap();
    let delivered = client.report_success().unwrap();
    assert_eq!(delivered.xml_attempts, 2);
    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    let first: XmlEnvelope = serde_xml_rs::from_str(&requests[2].body).unwrap();
    let second: XmlEnvelope =
        serde_xml_rs::from_str(&requests[4].body).unwrap();
    assert_eq!(first.incarnation, "1");
    assert_eq!(second.incarnation, "2");
}

#[test]
fn xml_errors_retain_the_original_json_failure() {
    let server = Server::new(vec![Reply::new(405, ""), Reply::new(403, "")]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            xml_fallback: true,
            ..server.config()
        },
    )
    .unwrap();
    let error = client.report_success().unwrap_err();
    assert!(error.to_string().contains("405"));
    assert!(error.to_string().contains("403"));
    assert!(error.source().is_some());
    let WireserverError::Fallback { json, xml } = error else {
        panic!("expected both protocol errors");
    };
    assert!(matches!(
        *json,
        WireserverError::HttpStatus {
            status: StatusCode::METHOD_NOT_ALLOWED,
            ..
        }
    ));
    assert!(matches!(
        *xml,
        WireserverError::HttpStatus {
            status: StatusCode::FORBIDDEN,
            ..
        }
    ));
    assert_eq!(server.requests().len(), 2);
}

#[rstest]
#[case("not XML")]
#[case("<GoalState><Incarnation>1</Incarnation></GoalState>")]
#[case("<GoalState><Incarnation>1</Incarnation><Container><ContainerId>id</ContainerId><RoleInstanceList/></Container></GoalState>")]
fn invalid_goal_state_is_explicit_and_never_posts_ready(#[case] body: &str) {
    let server = Server::new(vec![Reply::new(404, ""), Reply::new(200, body)]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            xml_fallback: true,
            ..server.config()
        },
    )
    .unwrap();
    let error = client.report_success().unwrap_err();
    let WireserverError::Fallback { xml, .. } = error else {
        panic!("expected an explicit fallback failure");
    };
    assert!(matches!(
        *xml,
        WireserverError::GoalState(_) | WireserverError::InvalidGoalState(_)
    ));
    assert_eq!(server.requests().len(), 2);
}

#[rstest]
#[case("bad\u{0}config".to_owned())]
#[case("&".repeat(900))]
fn xml_never_silently_truncates_or_repairs_a_failure(#[case] reason: String) {
    let server =
        Server::new(vec![Reply::new(404, ""), Reply::new(200, goalstate("1"))]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            xml_fallback: true,
            ..server.config()
        },
    )
    .unwrap();
    let report =
        ProvisioningReport::failure(AGENT, VM_ID, reason, ReportPpsType::None);
    let error = client.report_failure(&report).unwrap_err();
    let WireserverError::Fallback { xml, .. } = error else {
        panic!("expected XML validation failure");
    };
    assert!(matches!(*xml, WireserverError::InvalidReport(_)));
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn request_failures_are_bounded_and_do_not_select_xml() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            endpoint: format!("http://{address}/provisioning/health"),
            connect_timeout: Duration::from_millis(100),
            request_timeout: Duration::from_millis(100),
            total_retry_timeout: Duration::from_secs(1),
            retry_interval: Duration::ZERO,
            max_attempts: Some(2),
            xml_fallback: true,
            use_system_proxy: false,
        },
    )
    .unwrap();
    let error = client.report_success().unwrap_err();
    assert!(matches!(
        error,
        WireserverError::AttemptsExhausted { attempts: 2, .. }
    ));
    assert!(error.source().is_some());
}

#[test]
fn stalled_json_uses_the_total_budget_without_falling_back() {
    let server = Server::new(vec![
        Reply::new(200, "").delayed(Duration::from_millis(200))
    ]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            request_timeout: Duration::from_millis(20),
            total_retry_timeout: Duration::from_millis(60),
            max_attempts: None,
            xml_fallback: true,
            ..server.config()
        },
    )
    .unwrap();
    let started = Instant::now();
    let error = client.report_success().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(error, WireserverError::Timeout { .. }));
    assert!(error.source().is_some());
}

#[test]
fn fallback_shares_the_remaining_json_budget() {
    let server = Server::new(vec![
        Reply::new(404, "").delayed(Duration::from_millis(50)),
        Reply::new(200, goalstate("1")).delayed(Duration::from_millis(200)),
    ]);
    let client = WireserverClient::new(
        AGENT,
        WireserverConfig {
            request_timeout: Duration::from_secs(1),
            total_retry_timeout: Duration::from_millis(100),
            max_attempts: None,
            xml_fallback: true,
            ..server.config()
        },
    )
    .unwrap();
    let started = Instant::now();
    let error = client.report_success().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    let WireserverError::Fallback { xml, .. } = error else {
        panic!(
            "expected an XML timeout after JSON consumed part of the budget"
        );
    };
    assert!(matches!(*xml, WireserverError::Timeout { .. }));
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn rejects_non_failure_reports_without_io() {
    let server = Server::new(vec![]);
    let error = server
        .client()
        .report_failure(&report(ReportResult::Success))
        .unwrap_err();
    assert!(matches!(error, WireserverError::InvalidReport(_)));
    assert!(error.source().is_none());
    assert_eq!(server.requests().len(), 0);
}

#[rstest]
#[case("bad URL")]
#[case("https://example.test/provisioning/health")]
#[case("http://user:password@example.test/provisioning/health")]
#[case("http://example.test/provisioning/health#fragment")]
fn rejects_invalid_endpoints(#[case] endpoint: &str) {
    let error = WireserverClient::new(
        AGENT,
        WireserverConfig {
            endpoint: endpoint.into(),
            ..WireserverConfig::default()
        },
    )
    .unwrap_err();
    assert!(matches!(error, WireserverError::InvalidConfiguration(_)));
    assert!(error.to_string().contains("endpoint"));
}

#[rstest]
#[case("")]
#[case(" \t")]
#[case("bad\r\nheader")]
fn rejects_invalid_agents(#[case] agent: &str) {
    assert!(matches!(
        WireserverClient::new(agent, WireserverConfig::default()),
        Err(WireserverError::InvalidConfiguration(_))
    ));
}

#[rstest]
#[case("connect")]
#[case("request")]
#[case("total")]
#[case("attempts")]
#[case("unrepresentable")]
fn rejects_invalid_retry_settings(#[case] setting: &str) {
    let mut config = WireserverConfig::default();
    match setting {
        "connect" => config.connect_timeout = Duration::ZERO,
        "request" => config.request_timeout = Duration::ZERO,
        "total" => config.total_retry_timeout = Duration::ZERO,
        "attempts" => config.max_attempts = Some(0),
        "unrepresentable" => config.total_retry_timeout = Duration::MAX,
        _ => unreachable!(),
    }
    assert!(matches!(
        WireserverClient::new(AGENT, config),
        Err(WireserverError::InvalidConfiguration(_))
    ));
}

#[test]
fn builder_requires_a_destination_and_surfaces_http_setup_errors() {
    let error = HealthWriter::builder(AGENT, VM_ID).build().unwrap_err();
    assert!(matches!(error, HealthWriterBuildError::NoDestinations));
    assert!(error.source().is_none());
    assert!(error.to_string().contains("destination"));
    let error = HealthWriter::builder(AGENT, VM_ID)
        .wireserver(WireserverConfig {
            endpoint: "invalid".into(),
            ..WireserverConfig::default()
        })
        .build()
        .unwrap_err();
    assert!(matches!(error, HealthWriterBuildError::Wireserver(_)));
    assert!(error.source().is_some());
}

#[test]
fn kvp_only_writer_preserves_diagnostics_and_replaces_health() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(store.clone())
        .build()
        .unwrap();
    assert!(!store.path().exists());
    let in_progress = writer.report_in_progress(ReportPpsType::None).unwrap();
    assert!(in_progress.kvp_written);
    assert_eq!(in_progress.wireserver, None);
    let diagnostics =
        DiagnosticWriter::new(store.clone(), AGENT, VM_ID).unwrap();
    diagnostics
        .emit_event("demo", "operation", None, Some(Outcome::Ok), None)
        .unwrap();
    writer
        .report_failure("bad config", ReportPpsType::None)
        .unwrap();
    writer.report_success(ReportPpsType::None).unwrap();
    let stored: ProvisioningReport = store.entries().unwrap()
        [PROVISIONING_REPORT_KEY]
        .parse()
        .unwrap();
    assert_eq!(stored.result(), ReportResult::Success);
    assert_eq!(store.len().unwrap(), 2);
}

#[test]
fn both_destinations_receive_the_exact_same_failure_report() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let server = Server::new(vec![Reply::new(503, ""), Reply::new(200, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(store.clone())
        .wireserver(server.config())
        .build()
        .unwrap();
    let report = report(ReportResult::Error);
    let encoded = report.encode();
    let delivery = writer.write_report(&report).unwrap();
    assert!(delivery.kvp_written);
    assert_eq!(delivery.wireserver.unwrap().json_attempts, 2);
    assert_eq!(store.entries().unwrap()[PROVISIONING_REPORT_KEY], encoded);
    assert_eq!(store.len().unwrap(), 1);
    for request in server.requests() {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["details"]["description"], encoded);
    }
    assert_eq!(report.encode(), encoded);
}

#[test]
fn http_only_writer_needs_no_pool_or_daemon() {
    let server = Server::new(vec![Reply::new(200, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .wireserver(server.config())
        .build()
        .unwrap();
    let delivered = writer.report_success(ReportPpsType::None).unwrap();
    assert!(!delivered.kvp_written);
    assert_eq!(delivered.wireserver.unwrap().protocol, HealthProtocol::Json);
}

#[test]
fn kvp_failure_preserves_successful_http_delivery() {
    let dir = TempDir::new().unwrap();
    let store = KvpPoolStore::new_in(
        KvpPool::Guest,
        dir.path().join("missing"),
        PoolMode::Safe,
    )
    .unwrap();
    let server = Server::new(vec![Reply::new(200, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(store)
        .wireserver(server.config())
        .build()
        .unwrap();
    let error = writer
        .report_failure("bad config", ReportPpsType::None)
        .unwrap_err();
    assert!(error.source().is_some());
    assert!(error.to_string().contains("KVP"));
    assert!(matches!(
        error,
        HealthWriteError::Kvp {
            wireserver: Some(_),
            ..
        }
    ));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn http_failure_preserves_the_original_kvp_report() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let server = Server::new(vec![Reply::new(403, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(store.clone())
        .wireserver(server.config())
        .build()
        .unwrap();
    let report = report(ReportResult::Error);
    let error = writer.write_report(&report).unwrap_err();
    assert!(error.to_string().contains("403"));
    assert!(error.source().is_some());
    assert!(matches!(
        error,
        HealthWriteError::Wireserver {
            kvp_written: true,
            ..
        }
    ));
    assert_eq!(
        store.entries().unwrap()[PROVISIONING_REPORT_KEY],
        report.encode()
    );
}

#[test]
fn both_delivery_errors_are_returned() {
    let dir = TempDir::new().unwrap();
    let missing = KvpPoolStore::new_in(
        KvpPool::Guest,
        dir.path().join("missing"),
        PoolMode::Safe,
    )
    .unwrap();
    let server = Server::new(vec![Reply::new(403, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(missing)
        .wireserver(server.config())
        .build()
        .unwrap();
    let error = writer
        .report_failure("bad config", ReportPpsType::None)
        .unwrap_err();
    assert!(error.to_string().contains("KVP"));
    assert!(error.to_string().contains("403"));
    assert!(error.source().is_some());
    assert!(matches!(error, HealthWriteError::Both { .. }));
}

#[test]
fn single_target_errors_remain_explicit() {
    let dir = TempDir::new().unwrap();
    let missing = KvpPoolStore::new_in(
        KvpPool::Guest,
        dir.path().join("missing"),
        PoolMode::Safe,
    )
    .unwrap();
    let kvp = HealthWriter::builder(AGENT, VM_ID)
        .kvp(missing)
        .build()
        .unwrap();
    assert!(matches!(
        kvp.report_success(ReportPpsType::None),
        Err(HealthWriteError::Kvp {
            wireserver: None,
            ..
        })
    ));
    let server = Server::new(vec![Reply::new(403, "")]);
    let http = HealthWriter::builder(AGENT, VM_ID)
        .wireserver(server.config())
        .build()
        .unwrap();
    assert!(matches!(
        http.report_success(ReportPpsType::None),
        Err(HealthWriteError::Wireserver {
            kvp_written: false,
            ..
        })
    ));
}

#[test]
fn oversized_kvp_report_does_not_prevent_http() {
    let dir = TempDir::new().unwrap();
    let server = Server::new(vec![Reply::new(200, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(store(&dir))
        .wireserver(server.config())
        .build()
        .unwrap();
    let reason = "x".repeat(2048);
    let error = writer
        .report_failure(reason, ReportPpsType::None)
        .unwrap_err();
    assert!(matches!(
        error,
        HealthWriteError::Kvp {
            wireserver: Some(_),
            ..
        }
    ));
    let body: Value = serde_json::from_str(&server.requests()[0].body).unwrap();
    assert_eq!(body["state"], "NotReady");
    assert!(body["details"]["description"]
        .as_str()
        .unwrap()
        .contains(&"x".repeat(2048)));
}

#[test]
fn kvp_lock_contention_does_not_block_http_delivery() {
    let dir = TempDir::new().unwrap();
    let store = store(&dir);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(store.path())
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let server = Server::new(vec![Reply::new(200, "")]);
    let writer = HealthWriter::builder(AGENT, VM_ID)
        .kvp(store.clone())
        .wireserver(server.config())
        .build()
        .unwrap();
    let delivery =
        thread::spawn(move || writer.report_success(ReportPpsType::None));
    server.wait_for_requests(1);
    assert_eq!(std::fs::metadata(store.path()).unwrap().len(), 0);
    drop(lock);
    let delivery = delivery.join().unwrap().unwrap();
    assert!(delivery.kvp_written);
    assert_eq!(delivery.wireserver.unwrap().protocol, HealthProtocol::Json);
}
