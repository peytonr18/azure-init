// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Read and write Hyper-V KVP metadata on Linux guests.
//!
//! KVP (Key-Value Pair) data exchange lets a VM share small text records with
//! its host without network connectivity. Core APIs operate on local pool
//! files, normally in `/var/lib/hyperv`. The Hyper-V daemon and kernel handle
//! KVP transport; writing a file does not itself notify the host. The optional
//! `wireserver` feature adds a separate HTTP health-reporting transport.
//!
//! # Choose an API
//! - [`DiagnosticWriter`] emits operations and observations, handling
//!   timestamps, compression and splitting long payloads into records.
//! - [`DiagnosticReader`] reads native and cloud-init diagnostics, provisioning
//!   reports, and unrecognized records as [`Entry`] values.
//! - [`report_success`], [`report_failure`], and [`report_in_progress`] publish
//!   one replaceable provisioning state. Use [`ProvisioningReport`] and
//!   [`write_report`] for additional context or to reuse a report across
//!   transports.
//! - [`KvpPoolStore`] reads and writes ordinary key/value records. Use
//!   [`insert`](KvpPoolStore::insert) to update a key, or
//!   [`append`](KvpPoolStore::append) to preserve duplicates.
//! - With feature `wireserver`, `WireserverClient` publishes HTTP health and
//!   `HealthWriter` publishes to explicitly selected KVP/HTTP destinations.
//!   Neither installs a tracing subscriber or requires a caller-owned runtime.
//!
//! Use [`KvpPool::Guest`] for guest-produced telemetry and [`PoolMode::Safe`]
//! for host-readable writes. [`KvpPool::AutoExternal`] contains host-provided
//! data. The selected directory must exist and permit the requested file I/O;
//! [`KvpPoolStore::new_in`] selects an alternate directory.
//!
//! # Emit Telemetry
//!
//! ```no_run
//! use libazureinit_kvp::{
//!     DiagnosticWriter, Encoding, KvpPool, KvpPoolStore, Outcome, PoolMode,
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = KvpPoolStore::new(KvpPool::Guest, PoolMode::Safe)?;
//! let writer = DiagnosticWriter::new(
//!     store,
//!     "azure-init/0.1.1",
//!     "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
//! )?;
//! writer.emit_event(
//!     "imds", "metadata retrieved", None, Some(Outcome::Ok), None,
//! )?;
//! writer.emit_event(
//!     "os:release", std::fs::read("/etc/os-release")?,
//!     Some(Encoding::ZlibB64), None, None,
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! # Report Provisioning
//!
//! Reserve the report key before provisioning, then replace it with the final
//! result. These APIs write local KVP data, not HTTP health requests.
//!
//! ```no_run
//! use libazureinit_kvp::{
//!     report_in_progress, report_success, KvpPool, KvpPoolStore, PoolMode,
//!     ReportPpsType,
//! };
//!
//! # fn main() -> Result<(), libazureinit_kvp::KvpError> {
//! let store = KvpPoolStore::new(KvpPool::Guest, PoolMode::Safe)?;
//! let agent = "my-agent/1.0";
//! let vm_id = "vm-123";
//! report_in_progress(&store, agent, vm_id, ReportPpsType::None)?;
//! report_success(&store, agent, vm_id, ReportPpsType::None)?;
//! # Ok(())
//! # }
//! ```
//!
//! The CLI provides `report-in-progress`, `report-success`, and `report-failure`.
//! Its report commands use [`ProvisioningReport`] to support additional context.
//!
//! # Publish Health to KVP and HTTP
//!
//! Enable Cargo feature `wireserver`. These are blocking APIs: async callers
//! must construct, use and drop the HTTP client in a blocking worker.
//! The following example targets a loopback mock and an application-owned pool.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # #[cfg(feature = "wireserver")]
//! # {
//! use std::time::Duration;
//! use libazureinit_kvp::{
//!     HealthWriter, KvpPool, KvpPoolStore, PoolMode, ReportPpsType,
//!     WireserverConfig,
//! };
//!
//! let directory = std::env::temp_dir().join("health-example");
//! std::fs::create_dir_all(&directory)?;
//! let store = KvpPoolStore::new_in(KvpPool::Guest, directory, PoolMode::Safe)?;
//! let health = HealthWriter::builder(
//!     "my-agent/1.0", "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
//! )
//! .kvp(store)
//! .wireserver(WireserverConfig {
//!     endpoint: "http://127.0.0.1:8080/provisioning/health".into(),
//!     total_retry_timeout: Duration::from_secs(15),
//!     max_attempts: Some(3),
//!     xml_fallback: true,
//!     ..WireserverConfig::default()
//! })
//! .build()?;
//! let delivered = health.report_failure("invalid configuration", ReportPpsType::None)?;
//! println!("Health acknowledgement: {delivered:?}");
//! # }
//! # Ok(())
//! # }
//! ```
//!
//! Partial delivery is an error with the successful destination's result
//! retained. Callers must preserve their original provisioning error separately.
//! XML fallback is opt-in and only follows JSON HTTP 404, 405 or 501; it never
//! converts a failure report into `Ready`. See the [health reporting guide].
//!
//! # Format References
//! The [KVP contract] describes pool files and Hyper-V interfaces. The
//! [diagnostics contract] describes record fields and encodings for consumers
//! that read telemetry independently of this library.
//!
//! [KVP contract]: https://github.com/Azure/azure-init/blob/main/doc/kvp.md
//! [diagnostics contract]: https://github.com/Azure/azure-init/blob/main/doc/diagnostics.md
//! [health reporting guide]: https://github.com/Azure/azure-init/blob/main/doc/health-reporting.md

mod cli;
mod diagnostics;
mod error;
#[cfg(feature = "wireserver")]
mod health;
mod report;
mod store;
mod vm_id;
#[cfg(feature = "wireserver")]
mod wireserver;

pub use cli::run;
pub use diagnostics::{
    DecodeError, Diagnostic, DiagnosticEvent, DiagnosticFinish, DiagnosticKey,
    DiagnosticPayload, DiagnosticReader, DiagnosticStart, DiagnosticWriter,
    DurationPrecision, Encoding, Entry, Kind, Outcome, RawKeyValue,
    TimestampPrecision, DIAGNOSTIC_VERSION_ID, MAX_CHUNK_BYTES,
};
pub use error::KvpError;
#[cfg(feature = "wireserver")]
pub use health::{
    HealthDelivery, HealthWriteError, HealthWriter, HealthWriterBuildError,
    HealthWriterBuilder,
};
pub use report::{
    report_failure, report_in_progress, report_success, write_report,
    ProvisioningReport, ReportPpsType, ReportResult, PROVISIONING_REPORT_KEY,
};
pub use store::{KvpPool, KvpPoolStore, PoolMode};
pub use vm_id::read_vm_id;
#[cfg(feature = "wireserver")]
pub use wireserver::{
    HealthProtocol, WireserverClient, WireserverConfig, WireserverDelivery,
    WireserverError,
};
