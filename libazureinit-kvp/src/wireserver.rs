// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, USER_AGENT};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::json;

use crate::{ProvisioningReport, ReportResult};

const RETRY_CODES: &[u16] = &[400, 404, 410, 429, 500, 503];
const UNSUPPORTED_CODES: &[u16] = &[404, 405, 501];
const MAX_XML_DESCRIPTION_BYTES: usize = 4096;

/// Blocking WireServer delivery options. All timeouts use [`Duration`].
///
/// The retry budget is shared by JSON, goal-state lookup and XML fallback.
/// Clients bypass proxies by default and do not follow redirects. DHCP discovery
/// belongs to the caller; this configuration accepts the resolved endpoint.
#[derive(Clone, Debug)]
pub struct WireserverConfig {
    /// Complete JSON health URL. Must be HTTP, without credentials or a fragment.
    /// Defaults to `http://168.63.129.16/provisioning/health`.
    pub endpoint: String,
    /// Connection limit, also bounded by the request and remaining total budget.
    /// Defaults to 60 seconds.
    pub connect_timeout: Duration,
    /// Limit for each HTTP request, including reading a goal-state response.
    /// Defaults to 60 seconds and is clamped to the remaining total budget.
    pub request_timeout: Duration,
    /// Budget for one delivery call, including retries, waits and fallback.
    /// Defaults to 20 minutes. Must be positive.
    pub total_retry_timeout: Duration,
    /// Delay between retryable failures. Defaults to one second; zero is allowed.
    pub retry_interval: Duration,
    /// Optional attempt cap per protocol. `Some(1)` disables retries, but still
    /// permits unsupported-endpoint fallback. Each XML attempt fetches fresh
    /// goal state before its health POST. Defaults to no cap beyond the budget.
    pub max_attempts: Option<usize>,
    /// Opt in to XML only on JSON HTTP 404, 405 or 501.
    ///
    /// XML endpoints are derived from the JSON URL's origin:
    /// `/machine/?comp=goalstate` and `/machine/?comp=health`. Timeouts,
    /// authentication failures and 5xx other than 501 never select XML.
    /// Defaults to `false`.
    pub xml_fallback: bool,
    /// Explicitly allow reqwest's environment/system proxy discovery.
    ///
    /// Defaults to `false` for direct fabric traffic. The azure-init compatibility
    /// adapter enables this to preserve its preexisting proxy behavior.
    pub use_system_proxy: bool,
}

impl Default for WireserverConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://168.63.129.16/provisioning/health".into(),
            connect_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
            total_retry_timeout: Duration::from_secs(1200),
            retry_interval: Duration::from_secs(1),
            max_attempts: None,
            xml_fallback: false,
            use_system_proxy: false,
        }
    }
}

/// Protocol that acknowledged a health report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthProtocol {
    /// The primary `/provisioning/health` JSON protocol.
    Json,
    /// The legacy goal-state-based XML protocol.
    Xml,
}

/// Successful HTTP delivery, including observable fallback information.
///
/// An HTTP acknowledgement does not independently prove that Azure completed
/// the VM's provisioning workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireserverDelivery {
    /// Protocol that accepted the report.
    pub protocol: HealthProtocol,
    /// Number of JSON health POST attempts.
    pub json_attempts: usize,
    /// Number of XML attempts. Each starts with a goal-state GET and makes a
    /// health POST only if lookup and validation succeed.
    pub xml_attempts: usize,
    /// Unsupported JSON status that selected XML; absent for JSON delivery.
    pub fallback_status: Option<StatusCode>,
}

/// Configuration, transport or protocol failure during health reporting.
#[derive(Debug, thiserror::Error)]
pub enum WireserverError {
    /// Invalid endpoint, timeout, attempt count or agent header.
    #[error("invalid WireServer configuration: {0}")]
    InvalidConfiguration(String),
    /// An HTTP request or response-body read failed.
    #[error("WireServer request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The endpoint returned a non-success status.
    #[error("WireServer {endpoint} returned HTTP {status}")]
    HttpStatus {
        /// Endpoint that returned the status.
        endpoint: String,
        /// Received HTTP status.
        status: StatusCode,
        /// Original HTTP status error, when the status is 4xx or 5xx.
        #[source]
        source: Option<reqwest::Error>,
    },
    /// The shared delivery budget expired.
    #[error("WireServer delivery budget expired at {endpoint}")]
    Timeout {
        /// Endpoint for the protocol whose budget expired.
        endpoint: String,
        /// Last retryable failure, if any request was attempted.
        #[source]
        last_error: Option<Box<Self>>,
    },
    /// A protocol reached its configured attempt cap.
    #[error(
        "WireServer {endpoint} exhausted {attempts} attempts: {last_error}"
    )]
    AttemptsExhausted {
        /// Endpoint for the protocol that exhausted its attempts.
        endpoint: String,
        /// Number of attempts made.
        attempts: usize,
        /// Last retryable failure.
        #[source]
        last_error: Box<Self>,
    },
    /// The legacy goal-state response could not be decoded.
    #[error("invalid WireServer goal-state XML: {0}")]
    GoalState(#[from] serde_xml_rs::Error),
    /// Required legacy identifiers were empty or absent.
    #[error("invalid WireServer goal state: {0}")]
    InvalidGoalState(&'static str),
    /// A report could not be represented without changing its meaning.
    #[error("invalid WireServer health report: {0}")]
    InvalidReport(String),
    /// JSON was unsupported and XML delivery also failed.
    #[error("JSON reporting failed ({json}); XML fallback failed ({xml})")]
    Fallback {
        /// Unsupported JSON endpoint error.
        json: Box<Self>,
        /// XML lookup or delivery error.
        #[source]
        xml: Box<Self>,
    },
}

impl WireserverError {
    fn retryable(&self) -> bool {
        match self {
            Self::Request(error) => !error.is_builder(),
            Self::HttpStatus { status, .. } => {
                RETRY_CODES.contains(&status.as_u16())
            }
            _ => false,
        }
    }

    fn unsupported_status(&self) -> Option<StatusCode> {
        match self {
            Self::HttpStatus { status, .. }
                if UNSUPPORTED_CODES.contains(&status.as_u16()) =>
            {
                Some(*status)
            }
            _ => None,
        }
    }
}

/// Synchronous HTTP-only health reporter; no tracing subscriber is required.
///
/// Construction and use must occur outside an async runtime. Async callers
/// should construct, use and drop the client in a blocking worker.
/// Construction validates configuration but makes no network requests.
#[derive(Clone, Debug)]
pub struct WireserverClient {
    client: Client,
    config: WireserverConfig,
    endpoint: Url,
    agent: HeaderValue,
}

impl WireserverClient {
    /// Builds a client using the calling application's identity, not this
    /// library's package name or version.
    pub fn new(
        agent: &str,
        config: WireserverConfig,
    ) -> Result<Self, WireserverError> {
        let invalid = |message: &str| {
            WireserverError::InvalidConfiguration(message.to_owned())
        };
        let endpoint = Url::parse(&config.endpoint)
            .map_err(|error| invalid(&format!("endpoint: {error}")))?;
        if endpoint.scheme() != "http"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(invalid(
                "endpoint must be an HTTP URL without credentials or a fragment",
            ));
        }
        for (name, value) in [
            ("connect_timeout", config.connect_timeout),
            ("request_timeout", config.request_timeout),
            ("total_retry_timeout", config.total_retry_timeout),
        ] {
            if value.is_zero() || Instant::now().checked_add(value).is_none() {
                return Err(invalid(&format!(
                    "{name} must be positive and representable by the clock"
                )));
            }
        }
        if config.max_attempts == Some(0) {
            return Err(invalid("max_attempts must be at least one"));
        }
        if agent.trim().is_empty() {
            return Err(invalid("agent must not be empty"));
        }
        let agent = HeaderValue::from_str(agent)
            .map_err(|error| invalid(&format!("agent header: {error}")))?;
        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none());
        let client = if config.use_system_proxy {
            client
        } else {
            client.no_proxy()
        }
        .build()?;
        Ok(Self {
            client,
            config,
            endpoint,
            agent,
        })
    }

    /// Sends state-only `Ready`; does not write KVP.
    pub fn report_success(
        &self,
    ) -> Result<WireserverDelivery, WireserverError> {
        self.send(HealthState::Ready)
    }

    /// Sends `NotReady` / `ProvisioningFailed` with the supplied failure
    /// report's exact encoding. Never truncates or changes its timestamp.
    pub fn report_failure(
        &self,
        report: &ProvisioningReport,
    ) -> Result<WireserverDelivery, WireserverError> {
        if report.result() != ReportResult::Error {
            return Err(WireserverError::InvalidReport(
                "report_failure requires an error report".into(),
            ));
        }
        self.send(HealthState::Failed(report.encode()))
    }

    /// Sends `NotReady` / `Provisioning`; does not write KVP.
    pub fn report_in_progress(
        &self,
        vm_id: &str,
    ) -> Result<WireserverDelivery, WireserverError> {
        self.send(HealthState::InProgress(format!(
            "Provisioning is still in progress for vm_id={vm_id}."
        )))
    }

    /// Sends a preconstructed report without altering it.
    pub fn write_report(
        &self,
        report: &ProvisioningReport,
    ) -> Result<WireserverDelivery, WireserverError> {
        match report.result() {
            ReportResult::Success => self.report_success(),
            ReportResult::Error => self.report_failure(report),
            ReportResult::InProgress => self.report_in_progress(report.vm_id()),
        }
    }

    fn send(
        &self,
        state: HealthState,
    ) -> Result<WireserverDelivery, WireserverError> {
        let deadline = Deadline {
            started: Instant::now(),
            budget: self.config.total_retry_timeout,
        };
        let body = state.json().to_string();
        let mut json_attempts = 0;
        let json = self.retry(
            &deadline,
            &self.endpoint,
            &mut json_attempts,
            self.config.xml_fallback,
            || {
                self.request(
                    self.client
                        .post(self.endpoint.clone())
                        .headers(self.headers(false))
                        .body(body.clone()),
                    &self.endpoint,
                    &deadline,
                )?;
                Ok(())
            },
        );
        let json_error = match json {
            Ok(()) => {
                return Ok(WireserverDelivery {
                    protocol: HealthProtocol::Json,
                    json_attempts,
                    xml_attempts: 0,
                    fallback_status: None,
                });
            }
            Err(error)
                if self.config.xml_fallback
                    && error.unsupported_status().is_some() =>
            {
                error
            }
            Err(error) => return Err(error),
        };
        let fallback_status = json_error.unsupported_status();
        let mut goalstate_url = self.endpoint.clone();
        goalstate_url.set_path("/machine/");
        goalstate_url.set_query(Some("comp=goalstate"));
        let mut health_url = goalstate_url.clone();
        health_url.set_query(Some("comp=health"));
        let mut xml_attempts = 0;
        self.retry(&deadline, &health_url, &mut xml_attempts, false, || {
            let response = self.request(
                self.client
                    .get(goalstate_url.clone())
                    .headers(self.headers(true)),
                &goalstate_url,
                &deadline,
            )?;
            let bytes = response.bytes()?;
            let goalstate: GoalState =
                serde_xml_rs::from_reader(bytes.as_ref())?;
            let body = state.xml(&goalstate)?;
            self.request(
                self.client
                    .post(health_url.clone())
                    .headers(self.headers(true))
                    .body(body),
                &health_url,
                &deadline,
            )?;
            Ok(())
        })
        .map_err(|xml| WireserverError::Fallback {
            json: Box::new(json_error),
            xml: Box::new(xml),
        })?;
        Ok(WireserverDelivery {
            protocol: HealthProtocol::Xml,
            json_attempts,
            xml_attempts,
            fallback_status,
        })
    }

    fn headers(&self, xml: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, self.agent.clone());
        if xml {
            headers.insert("x-ms-agent-name", self.agent.clone());
            headers
                .insert("x-ms-version", HeaderValue::from_static("2012-11-30"));
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/xml; charset=utf-8"),
            );
        } else {
            headers.insert("x-ms-guest-agent-name", self.agent.clone());
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        headers
    }

    fn request(
        &self,
        request: reqwest::blocking::RequestBuilder,
        endpoint: &Url,
        deadline: &Deadline,
    ) -> Result<Response, WireserverError> {
        let remaining =
            deadline
                .remaining()
                .ok_or_else(|| WireserverError::Timeout {
                    endpoint: endpoint.to_string(),
                    last_error: None,
                })?;
        let response = request
            .timeout(self.config.request_timeout.min(remaining))
            .send()?;
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            Err(WireserverError::HttpStatus {
                endpoint: endpoint.to_string(),
                status,
                source: response.error_for_status().err(),
            })
        }
    }

    fn retry<T>(
        &self,
        deadline: &Deadline,
        endpoint: &Url,
        attempts: &mut usize,
        stop_on_unsupported: bool,
        mut operation: impl FnMut() -> Result<T, WireserverError>,
    ) -> Result<T, WireserverError> {
        let mut last_error = None;
        loop {
            let remaining = deadline.remaining().ok_or_else(|| {
                WireserverError::Timeout {
                    endpoint: endpoint.to_string(),
                    last_error: last_error.take(),
                }
            })?;
            if last_error.is_some() {
                thread::sleep(self.config.retry_interval.min(remaining));
            }
            if deadline.remaining().is_none() {
                return Err(WireserverError::Timeout {
                    endpoint: endpoint.to_string(),
                    last_error,
                });
            }
            *attempts = attempts.saturating_add(1);
            match operation() {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if (stop_on_unsupported
                        && error.unsupported_status().is_some())
                        || !error.retryable()
                    {
                        return Err(error);
                    }
                    if self
                        .config
                        .max_attempts
                        .is_some_and(|max| *attempts >= max)
                    {
                        return Err(WireserverError::AttemptsExhausted {
                            endpoint: endpoint.to_string(),
                            attempts: *attempts,
                            last_error: Box::new(error),
                        });
                    }
                    last_error = Some(Box::new(error));
                }
            }
        }
    }
}

struct Deadline {
    started: Instant,
    budget: Duration,
}

impl Deadline {
    fn remaining(&self) -> Option<Duration> {
        self.budget
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
    }
}

enum HealthState {
    Ready,
    Failed(String),
    InProgress(String),
}

impl HealthState {
    fn details(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::Ready => None,
            Self::Failed(description) => {
                Some(("ProvisioningFailed", description))
            }
            Self::InProgress(description) => {
                Some(("Provisioning", description))
            }
        }
    }

    fn state(&self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Failed(_) | Self::InProgress(_) => "NotReady",
        }
    }

    fn json(&self) -> serde_json::Value {
        match self.details() {
            Some((substatus, description)) => json!({
                "state": self.state(),
                "details": {
                    "subStatus": substatus,
                    "description": description,
                },
            }),
            None => json!({ "state": self.state() }),
        }
    }

    fn xml(&self, goalstate: &GoalState) -> Result<String, WireserverError> {
        let instance = goalstate.container.instances.roles.first().ok_or(
            WireserverError::InvalidGoalState("RoleInstanceList is empty"),
        )?;
        for value in [
            &goalstate.incarnation,
            &goalstate.container.id,
            &instance.id,
        ] {
            if value.trim().is_empty() {
                return Err(WireserverError::InvalidGoalState(
                    "incarnation, container and instance IDs must not be empty",
                ));
            }
        }
        let details = if let Some((substatus, description)) = self.details() {
            let description = escape_xml(description)?;
            if description.len() > MAX_XML_DESCRIPTION_BYTES {
                return Err(WireserverError::InvalidReport(format!(
                    "escaped XML description is {} bytes; maximum is {MAX_XML_DESCRIPTION_BYTES}",
                    description.len()
                )));
            }
            format!("<Details><SubStatus>{substatus}</SubStatus><Description>{description}</Description></Details>")
        } else {
            String::new()
        };
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <Health xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
             xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\
             <GoalStateIncarnation>{}</GoalStateIncarnation>\
             <Container><ContainerId>{}</ContainerId><RoleInstanceList>\
             <Role><InstanceId>{}</InstanceId><Health><State>{}</State>{details}\
             </Health></Role></RoleInstanceList></Container></Health>",
            escape_xml(&goalstate.incarnation)?,
            escape_xml(&goalstate.container.id)?,
            escape_xml(&instance.id)?,
            self.state(),
        ))
    }
}

fn escape_xml(value: &str) -> Result<String, WireserverError> {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\u{9}'
            | '\u{A}'
            | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}' => {
                escaped.push(character);
            }
            _ => {
                return Err(WireserverError::InvalidReport(
                    "value contains a character forbidden by XML 1.0".into(),
                ));
            }
        }
    }
    Ok(escaped)
}

#[derive(Deserialize)]
#[serde(rename = "GoalState")]
struct GoalState {
    #[serde(rename = "Incarnation")]
    incarnation: String,
    #[serde(rename = "Container")]
    container: Container,
}

#[derive(Deserialize)]
struct Container {
    #[serde(rename = "ContainerId")]
    id: String,
    #[serde(rename = "RoleInstanceList")]
    instances: RoleInstances,
}

#[derive(Deserialize)]
struct RoleInstances {
    #[serde(rename = "RoleInstance", default)]
    roles: Vec<RoleInstance>,
}

#[derive(Deserialize)]
struct RoleInstance {
    #[serde(rename = "InstanceId")]
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goalstate() -> GoalState {
        GoalState {
            incarnation: "1".into(),
            container: Container {
                id: "container".into(),
                instances: RoleInstances {
                    roles: vec![RoleInstance { id: "role".into() }],
                },
            },
        }
    }

    #[test]
    fn empty_legacy_identifiers_are_rejected() {
        for field in ["incarnation", "container", "instance"] {
            let mut context = goalstate();
            match field {
                "incarnation" => context.incarnation.clear(),
                "container" => context.container.id.clear(),
                "instance" => context.container.instances.roles[0].id.clear(),
                _ => unreachable!(),
            }
            assert!(matches!(
                HealthState::Ready.xml(&context),
                Err(WireserverError::InvalidGoalState(_))
            ));
        }
    }

    #[test]
    fn xml_text_preserves_unicode_and_valid_whitespace() {
        let value = "quotes \" ' \t\r\n\u{e000}\u{10000}";
        assert_eq!(escape_xml(value).unwrap(), value);
        assert_eq!(escape_xml("<&>").unwrap(), "&lt;&amp;&gt;");
        assert!(escape_xml("\u{fffe}").is_err());
    }

    #[test]
    fn an_expired_budget_never_starts_another_request() {
        let client = WireserverClient::new(
            "test",
            WireserverConfig {
                endpoint: "http://127.0.0.1:9/provisioning/health".into(),
                ..WireserverConfig::default()
            },
        )
        .unwrap();
        let deadline = Deadline {
            started: Instant::now(),
            budget: Duration::ZERO,
        };
        assert!(matches!(
            client.request(
                client.client.post(client.endpoint.clone()),
                &client.endpoint,
                &deadline
            ),
            Err(WireserverError::Timeout {
                last_error: None,
                ..
            })
        ));
        let mut attempts = 0;
        let result: Result<(), _> = client.retry(
            &deadline,
            &client.endpoint,
            &mut attempts,
            false,
            || panic!("an expired delivery must not run a request"),
        );
        assert!(matches!(
            result,
            Err(WireserverError::Timeout {
                last_error: None,
                ..
            })
        ));
        assert_eq!(attempts, 0);
    }
}
