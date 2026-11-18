# Afterburn / Azure-Init Health Reporting Demo Handoff

This is the implementation handoff for an agent working in the Afterburn
repository. The library work belongs to `peytonr18/azure-init`, on
`probertson-1.88-demo-branch`. The Afterburn code changes described below must be
made in Afterburn, not copied into azure-init.

## Goal and scope

Demonstrate Afterburn directly emitting KVP diagnostics and publishing explicit
provisioning health through the maintained `libazureinit-kvp` APIs:

- No tracing subscriber, tracing-to-KVP adapter or global logging replacement.
- JSON health reporting is primary.
- Optional XML fallback is entirely library-owned and activates only on JSON
  HTTP 404, 405 or 501.
- Replace Afterburn's own Azure readiness implementation, including its legacy
  XML construction, with library calls.
- Keep Afterburn's DHCP discovery for this demo. Whether to replace discovery
  is a later project decision; do not build a second DHCP client now.
- First prove Afterburn-owned success/failure paths in tests and a disposable VM.
  Complete OS-image and arbitrary Ignition-failure coverage are separate work.

The reviewed Afterburn baseline is commit
`64a44287acf4166cf19afffba93430a858357020`. Inspect the actual working branch
before editing in case it has advanced.

### Demo decision, not permanent XML ownership

The demo requires both success and failure reporting, including correct state
when falling back to XML. Keep the existing, locally tested library-owned
fallback for this demo; do not build a second XML formatter in Afterburn.
Azure-init's own compatibility adapter remains JSON-only.

This does not commit azure-init to maintaining the legacy protocol indefinitely.
Before upstreaming the design permanently, separately decide whether legacy
formatting/discovery should move to an Afterburn-owned adapter. Do not redesign
that boundary, move DHCP discovery, or expand legacy support during this demo.

## Library work available on the demo branch

Commit and push the completed azure-init changes before importing them from
another repository. The earlier merge commit `22ecb4d` predates these HTTP and
HealthWriter additions; do not pin that revision for the new APIs.

1. The workspace supports Rust 1.88. UUID requirements exclude releases
   requiring Rust 1.89.
2. Existing free report functions and the KVP CLI still write KVP only.
3. Feature `wireserver` adds:
   - `WireserverClient`: synchronous HTTP-only success/failure/in-progress APIs.
   - `WireserverConfig`: explicit endpoint and bounded retry options.
   - `HealthWriter`: KVP-only, HTTP-only or combined delivery.
   - Typed setup/delivery errors and observable JSON/XML acknowledgement data.
4. JSON formatting, unsupported-status detection, legacy goal-state lookup,
   XML formatting and retry/fallback policy are all in the library.
5. `read_vm_id()` provides a fallible, local, generation-corrected VM UUID lookup.
6. `ProvisioningReport` has read-only accessors, including `result()`,
   `vm_id()`, `timestamp()` and `extra()`.
7. Azure-init's existing async health wrappers delegate to this same transport.
   They retain JSON-only defaults and the original provisioning-result policy.

The full API contract is in [health-reporting.md](health-reporting.md) in the
azure-init repository
([demo-branch copy](https://github.com/peytonr18/azure-init/blob/probertson-1.88-demo-branch/doc/health-reporting.md)).
The important details are repeated here so the handoff
does not depend on carrying the entire source checkout between agents.

## Cargo setup

```toml
libazureinit-kvp = { git = "https://github.com/peytonr18/azure-init.git", branch = "probertson-1.88-demo-branch", features = ["wireserver"] }
```

Use the branch during development. For the final demo/image, replace `branch`
with the tested, pushed `rev` and commit Afterburn's lockfile. Verify the Git
revision includes these APIs before debugging a missing-feature error.
After the implementation is pushed, refresh an existing branch dependency with
`cargo update -p libazureinit-kvp`. Inspect the resulting Git revision in
`Cargo.lock`; merely editing the dependency's branch name does not prove the
latest commit was selected.

Retain Rust 1.88 compatibility. Afterburn's reviewed reqwest range already
permits 0.13; inspect the resolved graph and avoid gratuitously introducing
another HTTP stack or forcing a different TLS backend.

## Reporting setup

The following factory uses only the library and dependencies Afterburn already
has. The process must have access to the actual guest pool directory.

```rust,ignore
use std::path::Path;
use std::time::Duration;
use libazureinit_kvp::{
    DiagnosticWriter, HealthWriter, KvpPool, KvpPoolStore, PoolMode,
    WireserverConfig,
};

pub(crate) fn reporting(
    endpoint: String,
    pool_directory: &Path,
    vm_id: &str,
) -> anyhow::Result<(HealthWriter, Option<DiagnosticWriter>)> {
    let agent = concat!("afterburn/", env!("CARGO_PKG_VERSION"));
    let store =
        KvpPoolStore::new_in(KvpPool::Guest, pool_directory, PoolMode::Safe)?;
    let diagnostics = match DiagnosticWriter::new(store.clone(), agent, vm_id) {
        Ok(writer) => Some(writer),
        Err(error) => {
            slog_scope::warn!("Could not initialize KVP diagnostics: {error}");
            None
        }
    };
    let health = HealthWriter::builder(agent, vm_id)
        .kvp(store)
        .wireserver(WireserverConfig {
            endpoint,
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            total_retry_timeout: Duration::from_secs(15),
            retry_interval: Duration::from_millis(100),
            max_attempts: Some(3),
            xml_fallback: true,
            use_system_proxy: false,
        })
        .build()?;
    Ok((health, diagnostics))
}
```

These short timeouts are explicit demo choices, not recommended universal
production settings. Fit them within the relevant service's timeout.
Construct the production endpoint from Afterburn's discovered fabric address;
accept a complete URL here so loopback tests can inject an ephemeral port.

Use `read_vm_id()` when local DMI identity is available, or pass an already known
UUID. Do not silently invent one if discovery fails. The HTTP-only client can
report state without a successful IMDS lookup; do not make reporting depend on
the metadata operation whose failure is being reported.
Do not substitute a legacy role-instance ID or container ID for the VM UUID;
the library fetches those separate identifiers only when XML fallback needs them.

The factory above requires an explicit identity. If local identity lookup fails,
log that error and retain an HTTP-only reporting path rather than disabling all
health reporting. Health-report identity text is not required to be a UUID;
diagnostic identity is. Any unavailable-identity marker must be an explicit caller
policy, not an invented UUID. Log reporting-setup errors separately; do not let
the factory's `?` accidentally replace the underlying provisioning outcome.

The library does not create the pool directory. Ensure ownership, mount
availability and SELinux policy permit access. KVP writes can precede daemon
startup, but host retrieval requires the daemon/kernel integration and the same
pool file. An initramfs-only pool is not automatically visible after switch-root.

## Required Afterburn changes

### Replace readiness, not unrelated metadata transport

In `src/providers/microsoft/azure/mod.rs`, replace the Azure
`report_ready_state` / `ready_state!` health implementation with the library's
reporting options. The library must be the only health protocol/fallback owner.

Do not delete legacy goal-state/HTTP code still needed for other metadata
operations. Do not change Azure Stack or non-Azure providers as a side effect.
Initially make the modern Azure health path an explicit demo choice until
real-VM validation confirms the expected fabric outcome.

Remove the old outer health retry loop when it wraps the new library client.
Do not multiply the library's bounded retries through Afterburn's generic
`Retry`. Keep existing retry behavior for unrelated metadata and DHCP work.

### Capture the real error boundaries

Publishing only from `boot_checkin()` misses earlier failures.

Inspect and wrap the Azure-owned paths in:

- `src/cli/multi.rs`: provider construction, attributes, SSH keys, hostname,
  networking and final check-in.
- `src/cli/render_ignition.rs`: provider construction and fragment generation.
- `src/metadata.rs`: Azure provider construction can fail before an Azure object
  is returned.

Initialize a reporting context early enough to observe those failures. JSON
health reporting must not require successful legacy version negotiation or a
goal-state request. The library performs the latter only if XML fallback is
actually selected.

Use `DiagnosticWriter::emit_event`, or explicit `emit_start` / `emit_finish`
pairs with caller-owned UUIDs and timers. Retain Afterburn's slog logger for
delivery errors. There is no need to install a tracing subscriber.

At a real failure boundary, preserve the original error. The following helpers
also demonstrate a complete, explicitly owned success/failure flow with point
diagnostics and HTTP-required/KVP-best-effort final check-in:

```rust,ignore
use std::time::Instant;
use libazureinit_kvp::{
    DiagnosticWriter, HealthDelivery, HealthWriteError, HealthWriter, Outcome,
    ReportPpsType,
};

pub(crate) fn preserve_failure<T>(
    health: &HealthWriter,
    result: anyhow::Result<T>,
) -> anyhow::Result<T> {
    if let Err(original) = &result {
        match health.report_failure(
            format!("{original:#}"),
            ReportPpsType::None,
        ) {
            Ok(delivery) => {
                slog_scope::info!("Provisioning failure delivery: {delivery:?}");
            }
            Err(delivery) => {
                slog_scope::warn!(
                    "Failed to publish provisioning failure: {delivery}; destination details: {delivery:?}"
                );
            }
        }
    }
    result
}

pub(crate) fn run_owned_attempt<T>(
    health: &HealthWriter,
    diagnostics: Option<&DiagnosticWriter>,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if let Some(writer) = diagnostics {
        if let Err(error) =
            writer.emit_event("provision:begin", "starting attempt", None, None, None)
        {
            slog_scope::warn!("Failed to publish start diagnostic: {error}");
        }
    }

    let started = Instant::now();
    let result = operation();
    let duration = started.elapsed();
    if let Some(writer) = diagnostics {
        let (message, outcome) = if result.is_ok() {
            ("operation completed", Outcome::Ok)
        } else {
            ("operation failed", Outcome::Error)
        };
        if let Err(error) = writer.emit_event(
            "provision:finish", message, None, Some(outcome), Some(duration),
        ) {
            slog_scope::warn!("Failed to publish finish diagnostic: {error}");
        }
    }

    let value = preserve_failure(health, result)?;
    require_http_acknowledgement(health.report_success(ReportPpsType::None))?;
    Ok(value)
}

fn require_http_acknowledgement(
    result: Result<HealthDelivery, HealthWriteError>,
) -> anyhow::Result<()> {
    let acknowledgement = match result {
        Ok(HealthDelivery { wireserver: Some(delivery), .. }) => delivery,
        Ok(_) => anyhow::bail!("final check-in requires an HTTP destination"),
        Err(HealthWriteError::Kvp { error, wireserver: Some(delivery) }) => {
            slog_scope::warn!("KVP report failed, but HTTP acknowledged it: {error}");
            delivery
        }
        Err(error) => return Err(error.into()),
    };
    slog_scope::info!(
        "Health acknowledged: protocol={:?}, JSON attempts={}, XML attempts={}, fallback status={:?}",
        acknowledgement.protocol,
        acknowledgement.json_attempts,
        acknowledgement.xml_attempts,
        acknowledgement.fallback_status,
    );
    Ok(())
}
```

For the scoped demo, `run_owned_attempt` is the outer wrapper around one complete
owned provisioning flow whose successful end is final readiness. The closure
must not also call the old readiness API or publish a second success. Never wrap
each metadata action or `render-ignition` invocation with this success-publishing
helper. Those intermediate commands can use `preserve_failure` and diagnostic
calls without claiming VM readiness.

The example's begin/finish observations are independent point events with
automatically generated UUIDs, not an implicitly correlated operation pair.
For a paired operation, pass the same caller-generated UUID and name to
`emit_start` / `emit_finish`, and supply the measured duration explicitly.
Diagnostic failures are logged and do not replace the operation result.

The final success policy is deliberately HTTP-required and KVP-best-effort:

| Result | Example behavior |
|---|---|
| KVP and HTTP succeed | Return the original successful value |
| KVP fails, HTTP succeeds | Log the KVP error and return the successful value |
| HTTP fails, with or without a successful KVP write | Return the delivery error |
| The provisioning operation fails | Attempt failure reporting and return that original operation error, regardless of delivery outcome |

The library does not choose this policy for consumers. If Afterburn needs a
different policy, change it explicitly and test it rather than treating every
partial-delivery error as an HTTP failure.

Review/redact the error message before publication if it can contain secrets.
Never publish a complete Ignition configuration or credentials.

### Reserve KVP early without sending HTTP in-progress

The free `libazureinit_kvp::report_in_progress(&store, agent, vm_id, pps_type)`
function reserves the KVP report key only. Use it at the explicitly owned new
attempt boundary, before diagnostics can consume the pool's key budget, and log
any reservation error separately.

In contrast, `health.report_in_progress(pps_type)` targets every destination
selected on that `HealthWriter`, including HTTP. Do not interchange these calls
accidentally. The execution helper above deliberately does not reset the report
or emit HTTP in-progress on every invocation.

### Define lifecycle ownership

`PROVISIONING_REPORT` is one replaceable record per pool, not one per process.
Afterburn has multiple invocations and services.

- Publish in-progress once at an owned attempt boundary, before diagnostics
  consume pool capacity.
- Do not clear the pool or reset in-progress on every invocation.
- Do not overwrite an unrecovered failure from another invocation.
- Do not declare the entire VM ready just because a hostname or metadata
  subcommand succeeded.
- Keep overall success at the real final check-in boundary.
- Coordinate attempt/boot state across processes before claiming full boot
  coverage. Report accessors help inspect state, but the library does not
  supply an atomic cross-process lifecycle coordinator.

For an initial focused demo, explicitly bound which command owns the attempt.
For image-wide integration, design and test the cross-service policy rather
than assuming independent per-command wrappers are sufficient.

### Handle partial delivery explicitly

All selected destinations are attempted. KVP work runs independently of HTTP;
HTTP retries do not repeat KVP publication. The call waits for both results.

`HealthWriteError` variants:

- `Kvp { error, wireserver }`: HTTP may have succeeded. Its acknowledgement,
  including fallback information, remains available.
- `Wireserver { error, kvp_written }`: KVP may already contain the report.
- `Both { kvp, wireserver }`: neither selected destination succeeded.

For an already failed provisioning operation, return that original failure
regardless of delivery outcome.

For the check-in command, explicitly choose its delivery policy. To preserve
the old HTTP-required behavior, HTTP acknowledgement can remain sufficient
while KVP errors are logged. Do not treat every partial-delivery error as an
HTTP failure, and do not silently discard KVP errors.

Log a fallback acknowledgement using `WireserverDelivery.protocol`,
`fallback_status`, `json_attempts` and `xml_attempts`. Successful XML reporting
must remain visible as XML, not masquerade as JSON success.

## Fallback contract: do not reimplement it

- JSON is primary: `/provisioning/health`.
- Default library policy is JSON-only; `xml_fallback: true` opts in.
- Only JSON 404, 405 or 501 select legacy XML.
- Connection errors, timeouts, 401/403 and other 5xx (for example 500/503) do not
  select XML; 501 is the explicit unsupported-endpoint exception.
- The library derives legacy URLs from the JSON endpoint's origin:
  `/machine/?comp=goalstate`, then `/machine/?comp=health`.
- JSON and XML share the same total retry budget.
- `max_attempts` is a cap per protocol, including the initial attempt.
  Each XML attempt fetches fresh goal state; there are no nested request retries.
  The underlying client's automatic protocol-NACK retries are disabled.
- A failed provisioning report remains `NotReady` / `ProvisioningFailed` in
  both protocols. Delivery success never changes it into `Ready`.
- Failure descriptions reuse the exact encoded `ProvisioningReport`, including
  its original timestamp and supporting context.
- XML is escaped, not silently repaired or truncated. Invalid XML characters or
  descriptions exceeding 4,096 escaped UTF-8 bytes are explicit delivery errors.
- Proxy use is disabled by default; keep `use_system_proxy: false` for these
  fabric requests. The azure-init compatibility adapter explicitly retains its
  earlier environment-proxy behavior. Redirects are always disabled.

Do not keep a second XML template, fallback decision or retry loop in Afterburn.
DHCP/endpoint discovery remains separate and caller-owned for this demo.

## Tests and demo acceptance

1. Build and test Afterburn with Rust 1.88 and the pinned Git dependency.
2. Inject a temporary pool directory, known VM UUID and loopback health endpoint.
   The existing Afterburn mock HTTP client does not intercept requests from the
   new library client; configure both explicitly.
3. Verify success produces diagnostics, a KVP success report and JSON `Ready`.
4. Inject a failure Afterburn actually owns, such as malformed public-key
   metadata or a failed output operation. Verify a nonzero original outcome,
   diagnostic failure, KVP error report and JSON `NotReady` with the same reason.
5. Exercise JSON 404/405/501 and serve legacy goal state. Verify XML fallback,
   unchanged health state, escaped content and observable protocol metadata.
6. Verify 401/403, timeouts and other 5xx such as 500/503 do not activate fallback.
7. Verify KVP failure does not stop HTTP, and HTTP failure does not erase KVP.
8. Verify multiple actions/services cannot accidentally reset a prior failure
   or publish premature readiness.
9. Run in disposable VMs, using fresh first-boot success and failure cases.
   Capture pool contents, journals, delivery protocol and the actual Azure-side
   provisioning outcome. HTTP 2xx alone is not fabric-state verification.

   ### Loopback protocol fixtures

   Use the full mock URL, including its ephemeral port, for `endpoint`.

   | Scenario | Expected ordered requests and responses | Final state |
   |---|---|---|
   | JSON success | JSON health POST -> 200 | `Ready` |
   | JSON provisioning failure | JSON health POST -> 200 | `NotReady` / `ProvisioningFailed` |
   | XML success fallback | JSON POST -> 404, goal-state GET -> 200, XML health POST -> 200 | `Ready` |
   | XML failure fallback | JSON POST -> 404, goal-state GET -> 200, XML health POST -> 200 | `NotReady` / `ProvisioningFailed` |

   Repeat fallback tests with 405 and 501. HTTP 200 on the failure path acknowledges
   delivery of a failure report; it must not turn the provisioning operation into
   success. Use this minimal goal-state response for the library's fallback lookup:

   ```xml
   <GoalState>
     <Incarnation>1</Incarnation>
     <Container>
       <ContainerId>demo-container</ContainerId>
       <RoleInstanceList>
         <RoleInstance><InstanceId>demo-role</InstanceId></RoleInstance>
       </RoleInstanceList>
     </Container>
   </GoalState>
   ```

   In both protocols, decode the payload and compare its failure description with
   the KVP `PROVISIONING_REPORT` value, rather than asserting only that a request
   occurred. Use separate temporary pool directories for independent attempts.
   Do not run success immediately after failure in a shared pool and interpret the
   overwritten report as a lifecycle test.

   From an azure-init checkout, inspect a test pool with:

   ```sh
   cargo run --locked --package libazureinit-kvp -- --dir "$POOL_DIR" --pool guest dump --parse
   ```

   Set `POOL_DIR` to the actual test directory. Importing the library into Afterburn
   does not install the KVP CLI; it must be run from its checkout or built separately.
   In Afterburn tests, inspect `DiagnosticReader::new(store.clone()).entries()` and
   parse `store.entries()?[PROVISIONING_REPORT_KEY]` as a `ProvisioningReport`.

The azure-init-side validation commands are:

```sh
cargo +1.88.0 test --locked --package libazureinit-kvp --no-default-features
cargo +1.88.0 test --locked --all-features --workspace
cargo +1.88.0 clippy --locked --package libazureinit-kvp --all-targets --all-features -- --deny warnings
cargo +1.88.0 fmt --all --check
```

### Validation at handoff creation

- The Rust 1.88 all-feature workspace suite passed: 1,199 tests, with 5 ignored
  (including the Docker-dependent isolated provisioning test).
- The standalone KVP-only feature suite passed.
- Workspace build, standard CI Clippy and KVP all-target/all-feature Clippy passed.
- The factory and reporting-flow code blocks in this document were extracted,
  compiled and run as a separate Rust 1.88 consumer with fresh dependency
  resolution. It verified KVP diagnostics, JSON-to-XML `NotReady` delivery,
  the original error remaining intact, and proxy-free direct reporting even
  with deliberately invalid proxy environment variables.
- The expanded handoff flow and exact XML fixture were additionally checked by
  six standalone Rust 1.88 tests. They cover JSON success/failure, both states
  falling back on each of 404/405/501, diagnostic read-back, successful check-in
  despite KVP failure, failed check-in when HTTP fails, preservation of the
  original operation error when both delivery targets fail, rejection of a
  KVP-only final check-in, and health reporting when diagnostic identity is invalid.
- The documented KVP CLI inspection command was run successfully.
- The stricter whole-workspace/all-target Clippy command still reports the same
  18 preexisting findings in unrelated provisioning-library tests. Do not
  confuse those with new health-library findings or expand this demo to fix them.
- An actual Afterburn build, live Azure/fabric collection, full Ignition-failure
  coverage and the authoritative Codecov result are not verified by these
  local library checks.

Coverage acceptance remains 100% line coverage as reported by Codecov CI;
local test success is not a substitute for that report.

### Before switching repositories

- Commit and push the implementation and this handoff, then record the tested
  commit SHA. A local dirty worktree cannot be consumed by a remote Git dependency.
- Give the Afterburn agent this document and the
  [API guide](https://github.com/peytonr18/azure-init/blob/probertson-1.88-demo-branch/doc/health-reporting.md).
- Have it inspect the actual Afterburn branch and state which single flow owns
  the initial demo attempt before wiring final health publication.
- After integration, run both success and failure protocol matrices in Afterburn
  on Rust 1.88, then advance to disposable VM validation.

## Explicitly deferred

- Replacing or moving DHCP discovery.
- Fallback on arbitrary JSON failures instead of the selected unsupported-only
  policy.
- Assuming Azure Stack supports the new JSON protocol.
- Automatic detection of arbitrary Ignition failures outside Afterburn.
- Full image construction and broad platform rollout.

An Ignition fetch/parse/disk failure can occur outside an Afterburn process and
before normal check-in runs. Full coverage requires an explicit failure-reporting
entry point and initramfs/systemd ordering, including network/pool availability
before emergency isolation. Adding the library alone cannot observe such errors.
