// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async compatibility adapters for the shared, blocking health client.
//!
//! KVP publication and provisioning-result policy remain caller-owned. These
//! adapters preserve JSON-only delivery and the existing configuration format.

use libazureinit_kvp::{
    ProvisioningReport, WireserverClient, WireserverConfig, WireserverDelivery,
    WireserverError,
};
use std::io;
use std::time::Duration;
use tracing::instrument;

use crate::config::Wireserver;
use crate::error::Error;

/// Reports successful provisioning to the wireserver with a state-only request.
#[instrument(skip_all)]
pub async fn report_ready(config: &Wireserver) -> Result<(), Error> {
    tracing::info!("Reporting provisioning complete");
    deliver(config, |client| client.report_success()).await
}

/// Reports provisioning failure using the supplied report's exact encoding.
pub async fn report_failure(
    report: &ProvisioningReport,
    config: &Wireserver,
) -> Result<(), Error> {
    let report = report.clone();
    deliver(config, move |client| client.report_failure(&report)).await
}

/// Reports provisioning as still in progress over HTTP, without publishing KVP.
pub async fn report_in_progress(
    config: &Wireserver,
    vm_id: &str,
) -> Result<(), Error> {
    let vm_id = vm_id.to_owned();
    deliver(config, move |client| client.report_in_progress(&vm_id)).await
}

#[instrument(err, skip_all)]
async fn deliver(
    config: &Wireserver,
    operation: impl FnOnce(&WireserverClient) -> Result<WireserverDelivery, WireserverError>
        + Send
        + 'static,
) -> Result<(), Error> {
    let config = config.clone();
    let dispatcher = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _dispatcher = tracing::dispatcher::set_default(&dispatcher);
        let _entered = span.enter();
        let duration = |name, value| {
            Duration::try_from_secs_f64(value).map_err(|error| {
                shared_error(WireserverError::InvalidConfiguration(format!(
                    "{name}: {error}"
                )))
            })
        };
        let connect_timeout = duration(
            "connection_timeout_secs",
            config.connection_timeout_secs,
        )?;
        let total_retry_timeout = duration(
            "total_retry_timeout_secs",
            config.total_retry_timeout_secs,
        )?;
        if total_retry_timeout.is_zero() {
            return Err(Error::Timeout);
        }
        let client = WireserverClient::new(
            &format!("azure-init v{}", env!("CARGO_PKG_VERSION")),
            WireserverConfig {
                endpoint: config.health_endpoint,
                connect_timeout,
                request_timeout: duration(
                    "read_timeout_secs",
                    config.read_timeout_secs,
                )?,
                total_retry_timeout,
                retry_interval: connect_timeout,
                use_system_proxy: true,
                ..WireserverConfig::default()
            },
        )
        .map_err(shared_error)?;
        let delivered = operation(&client).map_err(shared_error)?;
        tracing::info!(
            target: "libazureinit::health::status",
            protocol = ?delivered.protocol,
            attempts = delivered.json_attempts,
            "WireServer acknowledged health report"
        );
        Ok(())
    })
    .await
    .map_err(|error| Error::Io(io::Error::other(error)))?
}

fn shared_error(error: WireserverError) -> Error {
    match error {
        WireserverError::Request(error)
        | WireserverError::HttpStatus {
            source: Some(error),
            ..
        } => Error::Http(error),
        WireserverError::HttpStatus {
            endpoint, status, ..
        } => Error::HttpStatus { endpoint, status },
        WireserverError::Timeout { .. } => Error::Timeout,
        WireserverError::GoalState(error) => Error::Xml(error),
        WireserverError::InvalidConfiguration(_)
        | WireserverError::InvalidGoalState(_)
        | WireserverError::InvalidReport(_) => {
            Error::Io(io::Error::new(io::ErrorKind::InvalidInput, error))
        }
        WireserverError::AttemptsExhausted { .. }
        | WireserverError::Fallback { .. } => {
            Error::Io(io::Error::other(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unittest::{get_http_response_payload, serve_requests};
    use reqwest::StatusCode;
    use rstest::rstest;
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn blocking_delivery_preserves_the_callers_tracing_context() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_requests(
            listener,
            get_http_response_payload(&StatusCode::OK, ""),
            cancel.clone(),
        ));
        report_ready(&fast_config(format!("http://{address}")))
            .await
            .unwrap();
        cancel.cancel();
        assert_eq!(server.await.unwrap(), 1);
        assert!(logs_contain("WireServer acknowledged health report"));
    }

    #[tokio::test]
    async fn invalid_timeout_values_are_errors_not_panics() {
        for value in [-1.0, f64::NAN, f64::INFINITY] {
            for field in ["connection", "request", "total"] {
                let mut config = Wireserver::default();
                match field {
                    "connection" => config.connection_timeout_secs = value,
                    "request" => config.read_timeout_secs = value,
                    "total" => config.total_retry_timeout_secs = value,
                    _ => unreachable!(),
                }
                assert!(matches!(
                    report_ready(&config).await,
                    Err(Error::Io(error))
                        if error.kind() == io::ErrorKind::InvalidInput
                ));
            }
        }
        let config = Wireserver {
            total_retry_timeout_secs: 0.0,
            ..Wireserver::default()
        };
        assert!(matches!(report_ready(&config).await, Err(Error::Timeout)));
    }

    #[tokio::test]
    async fn a_worker_panic_is_an_explicit_delivery_error() {
        let result = deliver(&Wireserver::default(), |_| {
            panic!("expected health worker panic")
        })
        .await;
        assert!(matches!(result, Err(Error::Io(_))));
    }

    #[test]
    fn shared_errors_preserve_the_compatibility_error_types() {
        let error = shared_error(WireserverError::HttpStatus {
            endpoint: "http://example.test/health".into(),
            status: StatusCode::FOUND,
            source: None,
        });
        assert!(matches!(
            error,
            Error::HttpStatus {
                status: StatusCode::FOUND,
                ..
            }
        ));
        let xml_error = serde_xml_rs::from_str::<String>("<").unwrap_err();
        assert!(matches!(
            shared_error(WireserverError::GoalState(xml_error)),
            Error::Xml(_)
        ));
        for error in [
            WireserverError::InvalidConfiguration("invalid".into()),
            WireserverError::InvalidGoalState("missing ID"),
            WireserverError::InvalidReport("invalid state".into()),
        ] {
            assert!(matches!(
                shared_error(error),
                Error::Io(error) if error.kind() == io::ErrorKind::InvalidInput
            ));
        }
        for error in [
            WireserverError::AttemptsExhausted {
                endpoint: "http://example.test/health".into(),
                attempts: 1,
                last_error: Box::new(WireserverError::InvalidReport(
                    "failed".into(),
                )),
            },
            WireserverError::Fallback {
                json: Box::new(WireserverError::InvalidReport("JSON".into())),
                xml: Box::new(WireserverError::InvalidReport("XML".into())),
            },
        ] {
            assert!(matches!(
                shared_error(error),
                Error::Io(error) if error.kind() == io::ErrorKind::Other
            ));
        }
    }

    fn fast_config(mock_url: String) -> Wireserver {
        Wireserver {
            connection_timeout_secs: 0.1,
            read_timeout_secs: 0.1,
            total_retry_timeout_secs: 0.5,
            health_endpoint: mock_url,
        }
    }

    /// Verifies that `_report` times out after multiple attempts
    /// for retryable HTTP statuses 400, 404, 410, 429, 500, and 503.
    #[rstest]
    #[case::bad_request(StatusCode::BAD_REQUEST)]
    #[case::not_found(StatusCode::NOT_FOUND)]
    #[case::gone(StatusCode::GONE)]
    #[case::too_many_requests(StatusCode::TOO_MANY_REQUESTS)]
    #[case::internal_server_error(StatusCode::INTERNAL_SERVER_ERROR)]
    #[case::service_unavailable(StatusCode::SERVICE_UNAVAILABLE)]
    #[tokio::test]
    async fn test_report_all_retryable(#[case] status: StatusCode) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock_url = format!("http://{addr}");
        let payload = get_http_response_payload(&status, "");
        let cancel = CancellationToken::new();
        let server =
            tokio::spawn(serve_requests(listener, payload, cancel.clone()));

        let cfg = fast_config(mock_url);
        let test_vm_id = "00000000-0000-0000-0000-000000000000";
        let err = Error::UnhandledError {
            details: "test_failure_retryable".to_string(),
        };
        let report = err.as_provisioning_report(test_vm_id);
        let result = report_failure(&report, &cfg).await;
        cancel.cancel();
        let requests = server.await.unwrap();
        assert!(
            matches!(&result, Err(Error::Timeout)),
            "expected a timeout after retrying HTTP {status}, got {result:?}"
        );
        assert!(
            requests >= 2,
            "HTTP {status} should cause multiple attempts, got {requests}"
        );
    }

    /// Verifies that `_report` succeeds immediately
    /// when the server responds with HTTP 200 OK or 201 Created on the first attempt.
    #[tokio::test]
    async fn test_report_immediate_success() {
        for status in [StatusCode::OK, StatusCode::CREATED] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let mock_url = format!("http://{addr}");
            let payload = get_http_response_payload(&status, "");
            let cancel = CancellationToken::new();
            let server =
                tokio::spawn(serve_requests(listener, payload, cancel.clone()));

            let cfg = fast_config(mock_url);
            let result = report_ready(&cfg).await;

            cancel.cancel();
            assert_eq!(server.await.unwrap(), 1);
            assert!(
                result.is_ok(),
                "{status} should be accepted as success, got {result:?}"
            );
        }
    }

    /// Verifies that `_report` fails after exactly one request
    /// for non-retryable HTTP statuses 401, 403, and 405.
    #[rstest]
    #[case::unauthorized(StatusCode::UNAUTHORIZED)]
    #[case::forbidden(StatusCode::FORBIDDEN)]
    #[case::method_not_allowed(StatusCode::METHOD_NOT_ALLOWED)]
    #[tokio::test]
    async fn test_report_non_retryable_codes(#[case] status: StatusCode) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock_url = format!("http://{addr}");
        let payload = get_http_response_payload(&status, "");
        let cancel = CancellationToken::new();
        let server =
            tokio::spawn(serve_requests(listener, payload, cancel.clone()));

        let cfg = fast_config(mock_url);
        let test_vm_id = "00000000-0000-0000-0000-000000000000";
        let err = Error::UnhandledError {
            details: "test_report_non_retryable_codes".to_string(),
        };
        let report = err.as_provisioning_report(test_vm_id);
        let result = report_failure(&report, &cfg).await;
        cancel.cancel();
        let requests = server.await.unwrap();
        assert_eq!(requests, 1, "HTTP {status} should not be retried");
        assert!(
            matches!(
                &result,
                Err(Error::Http(error)) if error.status() == Some(status)
            ),
            "expected HTTP {status}, got {result:?}"
        );
    }

    /// Reporting in progress should succeed on HTTP 200 or 201 immediately.
    #[tokio::test]
    async fn test_report_in_progress_immediate_success() {
        for status in [StatusCode::OK, StatusCode::CREATED] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let mock_url = format!("http://{addr}");
            let payload = get_http_response_payload(&status, "");
            let cancel = CancellationToken::new();
            let server =
                tokio::spawn(serve_requests(listener, payload, cancel.clone()));

            let cfg = fast_config(mock_url);
            let test_vm_id = "00000000-0000-0000-0000-000000000000";
            let result = report_in_progress(&cfg, test_vm_id).await;

            cancel.cancel();
            assert_eq!(server.await.unwrap(), 1);
            assert!(
                result.is_ok(),
                "{status} should be accepted as success, got {result:?}"
            );
        }
    }

    /// Exercise the public async adapters against an unresponsive endpoint.
    /// Here we point at an unresponsive loopback endpoint, and with tiny timeouts
    /// we expect both `report_ready` and `report_failure` to error out fast.
    #[tokio::test]
    async fn test_public_wrappers_error_on_dead_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut cfg = fast_config(format!("http://{addr}"));
        cfg = Wireserver {
            connection_timeout_secs: 0.01,
            read_timeout_secs: 0.01,
            total_retry_timeout_secs: 0.01,
            ..cfg
        };

        let test_vm_id = "00000000-0000-0000-0000-000000000000";
        let r1 = report_ready(&cfg).await;
        let err = Error::UnhandledError {
            details: "no config".to_string(),
        };
        let report = err.as_provisioning_report(test_vm_id);
        let r2 = report_failure(&report, &cfg).await;
        assert!(
            matches!(&r1, Err(Error::Timeout)),
            "report_ready should time out against an unresponsive server, got {r1:?}"
        );
        assert!(
            matches!(&r2, Err(Error::Timeout)),
            "report_failure should time out against an unresponsive server, got {r2:?}"
        );
    }
}
