// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{
    write_report, KvpError, KvpPoolStore, ProvisioningReport, ReportPpsType,
    WireserverClient, WireserverConfig, WireserverDelivery, WireserverError,
};

/// Successful delivery to every selected destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthDelivery {
    /// Whether a KVP report was written.
    pub kvp_written: bool,
    /// HTTP acknowledgement and fallback information, if HTTP was selected.
    pub wireserver: Option<WireserverDelivery>,
}

/// Invalid health-writer setup. Construction never publishes a report.
#[derive(Debug, thiserror::Error)]
pub enum HealthWriterBuildError {
    /// At least one destination must be explicitly selected.
    #[error("select at least one health-report destination")]
    NoDestinations,
    /// HTTP client configuration or construction failed.
    #[error("could not initialize the WireServer health client: {0}")]
    Wireserver(#[from] WireserverError),
}

/// One or both destinations failed. Successful partial delivery is retained.
///
/// The caller decides whether delivery errors affect its exit status. In a
/// provisioning error path, preserve the original provisioning error and log
/// this separate delivery error through the application's normal logger.
#[derive(Debug, thiserror::Error)]
pub enum HealthWriteError {
    /// KVP failed; HTTP was disabled or acknowledged the report.
    #[error("KVP health delivery failed: {error}")]
    Kvp {
        /// KVP validation or I/O error.
        #[source]
        error: KvpError,
        /// HTTP acknowledgement, if that destination was selected.
        wireserver: Option<WireserverDelivery>,
    },
    /// HTTP failed; KVP was disabled or successfully written.
    #[error("WireServer health delivery failed: {error}")]
    Wireserver {
        /// HTTP delivery error.
        #[source]
        error: WireserverError,
        /// Whether KVP was successfully written.
        kvp_written: bool,
    },
    /// Both destinations failed.
    #[error("KVP health delivery failed ({kvp}); WireServer delivery failed ({wireserver})")]
    Both {
        /// KVP validation or I/O error.
        kvp: KvpError,
        /// HTTP delivery error.
        #[source]
        wireserver: WireserverError,
    },
}

#[derive(Clone, Debug)]
enum Destinations {
    Kvp(KvpPoolStore),
    Wireserver(WireserverClient),
    Both(KvpPoolStore, WireserverClient),
}

/// Explicit, synchronous provisioning-health publisher.
///
/// Select KVP, WireServer, or both with [`builder`](Self::builder). When both
/// are selected, the KVP write runs on a scoped worker so file-lock contention
/// does not prevent HTTP delivery. The call waits for both results; KVP locking
/// retains the store's blocking semantics, while HTTP has its own retry budget.
///
/// Reports are constructed once and reused. HTTP retries never repeat the KVP
/// write. Errors do not roll back successful delivery to the other destination.
/// There is no background queue, tracing subscription, implicit cleanup or
/// automatic success/failure inference.
///
/// With HTTP enabled, construct, use and drop this writer outside async
/// runtimes, or inside a blocking worker.
#[derive(Clone, Debug)]
pub struct HealthWriter {
    agent: String,
    vm_id: String,
    destinations: Destinations,
}

impl HealthWriter {
    /// Starts configuration without I/O. Caller-supplied report identities are
    /// preserved; diagnostic writers separately require valid UUIDs.
    pub fn builder(
        agent: impl Into<String>,
        vm_id: impl Into<String>,
    ) -> HealthWriterBuilder {
        HealthWriterBuilder {
            agent: agent.into(),
            vm_id: vm_id.into(),
            kvp: None,
            wireserver: None,
        }
    }

    /// Publishes an explicitly started attempt. Call only at an owned attempt
    /// boundary: this replaces any previously stored final report.
    pub fn report_in_progress(
        &self,
        pps_type: ReportPpsType,
    ) -> Result<HealthDelivery, HealthWriteError> {
        self.write_report(&ProvisioningReport::in_progress(
            &self.agent,
            &self.vm_id,
            pps_type,
        ))
    }

    /// Publishes explicit provisioning success, not merely an operation's end.
    pub fn report_success(
        &self,
        pps_type: ReportPpsType,
    ) -> Result<HealthDelivery, HealthWriteError> {
        self.write_report(&ProvisioningReport::success(
            &self.agent,
            &self.vm_id,
            pps_type,
        ))
    }

    /// Publishes a failure, preserving the same report across both transports.
    pub fn report_failure(
        &self,
        reason: impl Into<String>,
        pps_type: ReportPpsType,
    ) -> Result<HealthDelivery, HealthWriteError> {
        self.write_report(&ProvisioningReport::failure(
            &self.agent,
            &self.vm_id,
            reason,
            pps_type,
        ))
    }

    /// Publishes an existing report without mutating it. KVP stores its encoding;
    /// HTTP failures carry that same encoding, while success and in-progress
    /// use their protocol-specific payloads. Headers identify this writer's caller.
    pub fn write_report(
        &self,
        report: &ProvisioningReport,
    ) -> Result<HealthDelivery, HealthWriteError> {
        match &self.destinations {
            Destinations::Kvp(store) => {
                write_report(store, report).map_err(|error| {
                    HealthWriteError::Kvp {
                        error,
                        wireserver: None,
                    }
                })?;
                Ok(HealthDelivery {
                    kvp_written: true,
                    wireserver: None,
                })
            }
            Destinations::Wireserver(client) => {
                let delivery =
                    client.write_report(report).map_err(|error| {
                        HealthWriteError::Wireserver {
                            error,
                            kvp_written: false,
                        }
                    })?;
                Ok(HealthDelivery {
                    kvp_written: false,
                    wireserver: Some(delivery),
                })
            }
            Destinations::Both(store, client) => {
                let (kvp, http) = std::thread::scope(|scope| {
                    let kvp = std::thread::Builder::new()
                        .spawn_scoped(scope, || write_report(store, report));
                    let http = client.write_report(report);
                    (
                        kvp.map_err(KvpError::Io).and_then(|worker| {
                            worker
                                .join()
                                .expect("KVP reporting worker panicked")
                        }),
                        http,
                    )
                });
                match (kvp, http) {
                    (Ok(()), Ok(delivery)) => Ok(HealthDelivery {
                        kvp_written: true,
                        wireserver: Some(delivery),
                    }),
                    (Err(error), Ok(delivery)) => Err(HealthWriteError::Kvp {
                        error,
                        wireserver: Some(delivery),
                    }),
                    (Ok(()), Err(error)) => Err(HealthWriteError::Wireserver {
                        error,
                        kvp_written: true,
                    }),
                    (Err(kvp), Err(wireserver)) => {
                        Err(HealthWriteError::Both { kvp, wireserver })
                    }
                }
            }
        }
    }
}

/// Destination configuration for [`HealthWriter`].
///
/// Select at least one destination before calling [`build`](Self::build).
/// Selecting HTTP validates and builds a blocking client at build time.
#[derive(Clone, Debug)]
pub struct HealthWriterBuilder {
    agent: String,
    vm_id: String,
    kvp: Option<KvpPoolStore>,
    wireserver: Option<WireserverConfig>,
}

impl HealthWriterBuilder {
    /// Selects the pool store. Its directory and permissions remain caller-owned.
    pub fn kvp(mut self, store: KvpPoolStore) -> Self {
        self.kvp = Some(store);
        self
    }

    /// Selects HTTP delivery with optional unsupported-endpoint XML fallback.
    pub fn wireserver(mut self, config: WireserverConfig) -> Self {
        self.wireserver = Some(config);
        self
    }

    /// Validates configuration without publishing or making network requests.
    pub fn build(self) -> Result<HealthWriter, HealthWriterBuildError> {
        let wireserver = self
            .wireserver
            .map(|config| WireserverClient::new(&self.agent, config))
            .transpose()?;
        let destinations = match (self.kvp, wireserver) {
            (Some(store), Some(client)) => Destinations::Both(store, client),
            (Some(store), None) => Destinations::Kvp(store),
            (None, Some(client)) => Destinations::Wireserver(client),
            (None, None) => return Err(HealthWriterBuildError::NoDestinations),
        };
        Ok(HealthWriter {
            agent: self.agent,
            vm_id: self.vm_id,
            destinations,
        })
    }
}
