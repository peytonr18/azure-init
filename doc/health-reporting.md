# Direct Provisioning Health APIs

The `libazureinit-kvp` APIs do not require a tracing subscriber. There are three
delivery choices:

| API | Destination |
|---|---|
| Existing free `report_in_progress`, `report_success`, `report_failure`, `write_report` functions | KVP only |
| `WireserverClient` | HTTP only |
| `HealthWriter` | Explicitly selected KVP, HTTP, or both |

`DiagnosticWriter` remains the API for diagnostic events and operation timings.
Clone the same `KvpPoolStore` into it and the health writer. Diagnostics do not
automatically change the provisioning result.

## Dependencies and runtime

Rust 1.88 is supported. HTTP reporting and `HealthWriter` require the optional
Cargo feature `wireserver`; default features remain empty.

```toml
libazureinit-kvp = { git = "https://github.com/peytonr18/azure-init.git", branch = "probertson-1.88-demo-branch", features = ["wireserver"] }
```

Commit and push the implementation before consuming the branch from another
repository. For reproducible demos, replace `branch` with the validated `rev`
and commit the consumer's lockfile.

These are synchronous, blocking APIs. They do not require a caller-owned Tokio
runtime. Do not construct, call, or drop the blocking HTTP client inside an async
runtime; put its entire lifetime in a blocking worker instead. Azure-init's
existing async WireServer functions provide that adapter.

## Setup and identity

1. Select pool 1 with `KvpPool::Guest` and host-compatible `PoolMode::Safe`.
2. Ensure the pool directory exists and the process can read/write the pool file.
   The store does not create its parent directory. New files have mode `0600`.
3. Supply the calling application's agent/version, not the library version.
4. Supply a VM ID. `read_vm_id()` performs a fallible local DMI lookup with
   Gen1 byte-order correction; it does not contact IMDS or invent an ID.
   Already known IDs can be supplied directly. `DiagnosticWriter` requires a
   valid UUID; provisioning reports preserve caller-supplied identity text.
5. Resolve any DHCP-derived fabric address in the calling application, then
   supply the complete JSON health URL.

Writing KVP does not require the integration daemon to be running. Host
retrieval does require the usual Hyper-V daemon/kernel integration, reading
the same pool file. In an initramfs, do not assume a temporary
`/var/lib/hyperv` is the pool the post-switch-root daemon will read.

## Minimal combined writer

The crate's generated documentation contains a complete, compile-checked
loopback example. The essential setup is:

```rust,ignore
let health = HealthWriter::builder(agent, vm_id)
    .kvp(store.clone())
    .wireserver(WireserverConfig {
        endpoint,
        xml_fallback: true,
        ..WireserverConfig::default()
    })
    .build()?;
```

Select one or both destinations. Selecting neither returns
`HealthWriterBuildError::NoDestinations`. Construction validates configuration
without publishing or making network requests.

The writer provides:

- `report_in_progress(pps_type)`
- `report_success(pps_type)`
- `report_failure(reason, pps_type)`
- `write_report(&report)` for a preconstructed `ProvisioningReport`, including
  optional documentation and supporting fields

For a complete success/failure execution wrapper with diagnostic calls and
explicit partial-delivery policy, see the
[Afterburn handoff examples](afterburn-demo-handoff.md#capture-the-real-error-boundaries).

`ReportPpsType::None` is appropriate when no pre-provisioning state applies.
Report accessors expose the state, identity, timestamp, PPS type, reason,
documentation URL and ordered supporting data without parsing encoded CSV.

Reserve the report key before diagnostics fill the pool, but only at an
explicitly owned attempt boundary. In-progress publication replaces any prior
final report. The library does not decide when a new attempt begins, clear
previous-boot data, latch failures across processes, or infer success on drop.

## WireServer options

All duration fields use `std::time::Duration`.

| Field | Default | Contract |
|---|---|---|
| `endpoint` | `http://168.63.129.16/provisioning/health` | Complete HTTP JSON URL; credentials and fragments are rejected |
| `connect_timeout` | 60 seconds | Positive connection limit; the request/remaining budget may impose a shorter limit |
| `request_timeout` | 60 seconds | Positive per-request limit, including goal-state body reads; clamped to the remaining budget |
| `total_retry_timeout` | 1,200 seconds | Positive shared budget for one report, including both protocols and retry waits |
| `retry_interval` | 1 second | Fixed delay between retryable attempts; zero is permitted |
| `max_attempts` | `None` | Optional positive cap per protocol; `Some(1)` disables retries, not fallback |
| `xml_fallback` | `false` | Enable XML only for JSON HTTP 404, 405, or 501 |
| `use_system_proxy` | `false` | Explicit opt-in to reqwest environment/system proxy discovery; leave disabled for direct fabric traffic |

For tests and demos, set a short total budget explicitly rather than inheriting
the 20-minute production default. For example, use a 1-second connection limit,
2-second request limit, 15-second total budget, 100-millisecond interval and
`Some(3)` attempts. Fit the total budget within the service's execution budget.

The first attempt is included in `max_attempts`. Each protocol has its own cap,
but both share the same deadline. An XML attempt includes a fresh goal-state
GET followed by its health POST. There is no nested retry loop per HTTP request.
The underlying client's automatic protocol-NACK retries are disabled.

Transport errors and HTTP 400, 404, 410, 429, 500 and 503 are retryable, preserving
the existing azure-init health policy. Terminal responses, including 401 and 403,
are returned immediately. With fallback enabled, JSON 404/405/501 select XML
immediately instead of being retried as JSON.

The client bypasses environment/system proxies unless explicitly opted in, and
does not follow redirects.
It does not perform DHCP discovery. HTTP budgets do not bound an application's
separate DHCP or metadata lookup.

## JSON and XML ownership

For this demo, all protocol and fallback behavior belongs to this library.
XML is opt-in demo compatibility for both success and failure, not a decision
to maintain legacy WireServer support permanently. Reassess long-term ownership
separately before upstreaming; do not add another formatter in the demo consumer.

The current flow is:

1. POST JSON to the configured endpoint.
2. If and only if fallback is enabled and JSON returns 404, 405 or 501, use the
   remaining budget to GET `/machine/?comp=goalstate` at the same origin.
3. Extract incarnation, container ID and role-instance ID.
4. POST legacy XML to `/machine/?comp=health` at that origin.
5. Fetch fresh goal state before each retried XML attempt.

No legacy lookup occurs on a successful JSON path. Timeouts, connection errors,
authentication failures and 5xx other than the explicit 501 exception never
activate fallback. Fallback failure retains both the unsupported JSON error and
the XML error.

The state mapping is the same in both protocols:

| Report | State | Details |
|---|---|---|
| Success | `Ready` | No details |
| Failure | `NotReady` | `ProvisioningFailed`; description is the exact encoded report |
| In progress | `NotReady` | `Provisioning`; description identifies the VM |

XML text is escaped without altering its decoded content. Invalid XML 1.0
characters and descriptions exceeding 4,096 escaped UTF-8 bytes return an
explicit error instead of being silently repaired or truncated. JSON does not
inherit that legacy XML bound. KVP has its separate, smaller single-value limit.

`WireserverDelivery` reports the accepting `HealthProtocol`, JSON and XML
attempt counts, and the unsupported JSON status when fallback was used.
The attempt counters count protocol attempts, not individual HTTP requests:
an XML attempt can make two requests.

An HTTP acknowledgement is not proof of the Azure-side provisioning outcome.
Validate that separately on fresh VMs.

## Partial delivery and error policy

When both destinations are selected, the KVP write runs on a scoped worker so
its file lock does not prevent HTTP delivery. The call waits for both results.
KVP locking retains its existing blocking semantics; the HTTP timeout is not
a KVP lock timeout.

The report is constructed once. KVP is written once; failure HTTP retries reuse
the same encoded report and timestamp. Success and in-progress HTTP payloads
follow the state mapping above. Delivery is not transactional, and a successful
destination is not rolled back.

`HealthWriteError` distinguishes:

- `Kvp { error, wireserver }`: KVP failed; HTTP was disabled or acknowledged.
- `Wireserver { error, kvp_written }`: HTTP failed; KVP was disabled or written.
- `Both { kvp, wireserver }`: both selected destinations failed.

Successful partial HTTP delivery, including XML fallback information, remains
available in the error. `HealthDelivery` is returned only when every selected
destination succeeded.

In a provisioning failure handler, log delivery problems separately and return
the original provisioning failure. Do not use a reporting `?` that replaces the
original error. For a check-in command, the application may retain its existing
HTTP-required/KVP-best-effort policy by inspecting these typed results.

Review/redact failure context before publication. Do not publish raw Ignition
configuration, passwords or credentials as error context.

## Existing azure-init and CLI behavior

Azure-init's async adapters use the shared HTTP client on a blocking worker,
including client construction and destruction. Their existing configuration
format, JSON-only default, environment-proxy support and retry interval mapping
are preserved. In that compatibility adapter, the retry interval remains the
connection timeout. Redirects are rejected rather than followed, including by
the compatibility adapter.

The provisioning orchestrator still publishes its in-progress reservation to
KVP only, publishes final KVP and HTTP reports independently, and does not
replace the actual provisioning result with a reporting failure.

The existing KVP CLI report commands remain KVP-only even when the HTTP Cargo
feature is enabled. No CLI command silently starts sending HTTP reports.
