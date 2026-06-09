# `libazureinit-kvp` diagnostics architecture

This document describes the diagnostics + tracing layers that will be added
on top of the existing [`store`](src/store.rs) module, and the CLI binary
that exposes the crate's public APIs for operational use.

It is the design that will eventually replace
[`libazureinit/src/kvp.rs`](../libazureinit/src/kvp.rs) and the KVP-related
parts of [`libazureinit/src/logging.rs`](../libazureinit/src/logging.rs).
Those modules are **not** modified by this work; they continue to function
as-is until a dedicated wiring step at the end of the migration switches
[`src/main.rs`](../src/main.rs) over to the new APIs.

---

## 1. Goals

1. Provide a typed view (`DiagnosticsKvp`) over `KvpPoolStore` that owns:
   - the event-key format used by azure-init,
   - value chunking for messages larger than one KVP record,
   - reassembly when reading.
2. Provide a `tracing_subscriber::Layer` (`KvpTracingLayer`) that turns
   `tracing` span/event activity into `DiagnosticsKvp` writes.
3. Provide a CLI binary that exercises the crate's public APIs for
   inspection and troubleshooting.
4. Stay inside the existing crate. No new workspace members, no new
   default dependencies, no async runtime.
5. Be a clean, simple replacement for the legacy modules — drop the
   complexity that was only needed because there was no underlying
   storage abstraction.

## 2. Non-goals

- Changing `KvpPoolStore`'s wire format.
- Touching `libazureinit/src/kvp.rs` or `libazureinit/src/logging.rs`.
- Touching `src/main.rs`.
- Adding tokio or any other async runtime to this crate.
- Preserving the legacy `Kvp` struct shape (`Option<Layer>`, writer
  `JoinHandle`, `CancellationToken`, mpsc channel, batched writes).
  All of that exists to work around problems `KvpPoolStore` solves.

`KvpPoolStore`'s public API gains two additive methods
(`append_many`, `delete_many`) to support atomic multi-record
operations needed by the diagnostics layer. See [§4.1](#41-additions-to-kvppoolstore).

---

## 3. Crate layout

```
libazureinit-kvp/
├── Cargo.toml
├── diagnostics-architecture.md       (this file)
├── README.md                         (new)
├── examples/
│   ├── emit_event.rs                 (new)
│   └── tracing_setup.rs              (new)
└── src/
    ├── lib.rs                        (existing — extended re-exports)
    ├── error.rs                      (existing — unchanged)
    ├── store.rs                      (existing — unchanged)
    ├── diagnostics/                  (new module)
    │   ├── mod.rs                    DiagnosticsKvp, DiagnosticRecord
    │   ├── event.rs                  DiagnosticEvent, timestamp formatting
    │   ├── key.rs                    EventKey (format + parse)
    │   ├── chunk.rs                  chunk_at_char_boundary
    │   └── tracing.rs                KvpTracingLayer  (feature: tracing-layer)
    ├── cli/                          (new module, feature: cli)
    │   ├── mod.rs                    Cli, Command, run()
    │   └── json.rs                   JSON output helpers
    └── bin/
        └── azure-init-kvp.rs         (new binary; required-features = ["cli"])
```

### Feature flags

```toml
[features]
default      = ["diagnostics"]
diagnostics  = []
tracing-layer = ["diagnostics", "dep:tracing-subscriber", "dep:uuid"]
serde        = ["dep:serde", "dep:serde_json"]
cli          = ["diagnostics", "serde", "dep:clap", "dep:anyhow"]
```

The library compiles cleanly with **no** features (just the storage layer).
Embedders that want the tracing layer turn on `tracing-layer`; the bundled
CLI binary requires `cli` and is built with `cargo build --features cli`.

Removed from the previous proposal: the standalone `azure-init-kvp` crate.
The CLI lives in `src/bin/azure-init-kvp.rs` inside this crate.

---

## 4. Existing layer (recap)

[`KvpPoolStore`](src/store.rs) is the storage primitive. The diagnostics
layer composes its public methods and nothing more:

| Need | Method used |
|---|---|
| Write a chunked event atomically | `append_many(records)` *(new — see §4.1)* |
| Write a one-off raw record | `append(key, value)` |
| Read everything in order | `dump()` (preserves duplicates) |
| Detect and clear stale pool on startup | `clear_if_stale()` |
| Bulk-delete this layer's keys | `delete_many(keys)` *(new — see §4.1)* |
| Delete one key | `delete(key)` |
| Replace contents atomically | `populate(records)` |

`KvpPoolStore` already enforces the 254/1022-byte safe-mode caps, the
1024-unique-key cap, `flock`+`fcntl` locking, and stale-file detection.
The diagnostics layer adds no new I/O of its own — it is a thin policy
layer.

### 4.1 Additions to `KvpPoolStore`

Two additive methods, modeled on the existing `populate` (one lock,
many records):

```rust
impl KvpPoolStore {
    /// Append every record under a single exclusive lock. All records
    /// are written contiguously on disk, in the supplied order, with
    /// no other writer able to interleave between them.
    ///
    /// This is the primitive `DiagnosticsKvp::emit` uses to write
    /// multi-chunk events: each chunk is one record, all sharing the
    /// same key, all guaranteed adjacent so reassembly via `dump()`
    /// is correct under concurrent writers.
    ///
    /// Validation (key/value size, null bytes, `MAX_UNIQUE_KEYS`)
    /// happens before the lock is taken; a rejected batch is a no-op.
    pub fn append_many<I, K, V>(&self, records: I) -> Result<(), KvpError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>;

    /// Delete every record whose key appears in `keys`, under a
    /// single exclusive lock. Returns the number of records removed
    /// (each matching key may remove multiple records if `append`/
    /// `append_many` produced duplicates).
    ///
    /// O(N · K) where N = records on disk, K = keys to delete, but
    /// only one lock acquisition and one truncation regardless.
    pub fn delete_many<I, K>(&self, keys: I) -> Result<usize, KvpError>
    where
        I: IntoIterator<Item = K>,
        K: AsRef<str>;
}
```

**Why these belong in the store, not the diagnostics layer.** Both
operations need to hold the file lock across multiple records.
`KvpPoolStore` owns the lock; exposing handles or lock primitives
would break its encapsulation. `append_many` is the natural
companion to `populate` (atomic batch write, additive vs. replacing)
and uses the same `KvpPoolIter` mechanics already in
[store.rs](src/store.rs).

---

## 5. `diagnostics` module

### 5.1 `DiagnosticEvent`

```rust
// src/diagnostics/event.rs

use std::time::SystemTime;

/// A single diagnostic event. Maps to one logical entry; if `message`
/// is larger than [`MAX_CHUNK_BYTES`] it lands as multiple KVP records
/// sharing the same key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub level: tracing::Level,
    /// e.g. "provision:user:create_user"
    pub name: String,
    /// Per-emit identifier. The tracing layer generates a fresh UUIDv4
    /// for every emitted record (matching legacy behavior); it is
    /// **not** a span identity. Two records produced for the same
    /// logical span (on_event + on_close) will have different
    /// `event_id`s.
    pub event_id: String,
    /// Literal value bytes as written to the pool. Callers (i.e., the
    /// tracing layer) construct this — e.g. `"Time: ... | Event: ..."`
    /// for in-span events or `"Start: ... | End: ..."` for span
    /// closures. The library does not impose a value-string format.
    pub message: String,
    /// When the event was generated. Structured metadata used by the
    /// CLI (e.g. `--since` filtering) and JSON output. The library
    /// does **not** stamp this into `message` automatically.
    pub timestamp: SystemTime,
}

impl DiagnosticEvent {
    pub fn new(
        level: tracing::Level,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self;

    pub fn with_event_id(self, event_id: impl Into<String>) -> Self;
    pub fn with_timestamp(self, ts: SystemTime) -> Self;
}
```

Notes:

- Uses `tracing::Level` directly (already a non-optional dep). No
  parallel `enum Level` to maintain.
- `timestamp` is `SystemTime`, not `DateTime<Utc>`. We add no `chrono`
  dependency; an internal helper formats RFC3339 with millisecond
  precision (≈30 lines, well-tested). Used only for CLI/JSON output.
- No `Display` impl. `message` is the literal value written; if a
  caller wants the legacy `"Time: ... | Event: ..."` rendering they
  build it themselves and put it in `message`. This keeps
  `DiagnosticEvent` honest about being a structured record.

### 5.2 `EventKey`

```rust
// src/diagnostics/key.rs

/// `<prefix>|<vm_id>|<level>|<name>|<event_id>`
pub struct EventKey<'a> {
    pub prefix: &'a str,
    pub vm_id: &'a str,
    pub level: tracing::Level,
    pub name: &'a str,
    pub event_id: &'a str,
}

impl<'a> EventKey<'a> {
    pub fn format(&self) -> String;
    pub fn parse(key: &'a str) -> Option<EventKey<'a>>;
}
```

`format` is the inverse of `parse`. `parse` returns `None` for any key
that does not have exactly five `|`-separated segments — that's how the
diagnostics layer distinguishes its own events from `Raw` records like
`PROVISIONING_REPORT`.

### 5.3 Chunking

```rust
// src/diagnostics/chunk.rs

/// Maximum value bytes per record under `PoolMode::Safe`. Matches
/// `KvpPoolStore::max_value_size()` for a safe-mode store.
pub const MAX_CHUNK_BYTES: usize = 1022;

/// Split a string into pieces of at most `max_bytes` bytes each,
/// always at UTF-8 codepoint boundaries.
pub(crate) fn chunk_at_char_boundary(s: &str, max_bytes: usize) -> Vec<&str>;
```

**Why chunking exists.** A single `KvpPoolStore::append` writes exactly
one fixed-width record (1022-byte value cap in safe mode). The Azure
host surfaces multiple records that share the same key as a sequence;
clients reassemble them in on-disk order. Legacy code in
[`libazureinit/src/kvp.rs`](../libazureinit/src/kvp.rs) does the same.

**Why it lives here, not in `KvpPoolStore`.** Keeping `KvpPoolStore`'s
"one call = one record" contract means oversized writes from other code
paths (config dumps, raw KVPs) still fail loudly with `ValueTooLarge`
instead of being silently multiplexed.

**Why UTF-8 boundary splitting.** Splitting mid-codepoint produces
invalid UTF-8 in a record. The helper backs off to the previous
boundary; pathological inputs (single 5-byte codepoint into a 4-byte
window) are not a real concern because `MAX_CHUNK_BYTES = 1022` is
orders of magnitude larger than any codepoint.

### 5.4 `DiagnosticsKvp`

```rust
// src/diagnostics/mod.rs

use crate::{KvpError, KvpPoolStore};

#[derive(Clone, Debug)]
pub struct DiagnosticsKvp {
    store: KvpPoolStore,
    vm_id: String,
    event_prefix: String,
}

impl DiagnosticsKvp {
    pub fn new(
        store: KvpPoolStore,
        vm_id: impl Into<String>,
        event_prefix: impl Into<String>,
    ) -> Self;

    pub fn store(&self) -> &KvpPoolStore;
    pub fn vm_id(&self) -> &str;
    pub fn event_prefix(&self) -> &str;

    /// Write `event`. Messages longer than [`MAX_CHUNK_BYTES`] are
    /// split at UTF-8 codepoint boundaries and written as multiple
    /// records sharing the same key, atomically under a single lock
    /// via [`KvpPoolStore::append_many`].
    pub fn emit(&self, event: &DiagnosticEvent) -> Result<(), KvpError>;

    /// Write an unstructured key/value pair (e.g. `PROVISIONING_REPORT`).
    /// "Raw" means no event-key formatting, **not** no chunking:
    /// long values are split the same way `emit` splits messages,
    /// using [`KvpPoolStore::append_many`].
    pub fn emit_raw(&self, key: &str, value: &str) -> Result<(), KvpError>;

    /// Read all records, reassembling chunked events.
    pub fn records(&self) -> Result<Vec<DiagnosticRecord>, KvpError>;

    /// Only the records that parsed as `DiagnosticEvent`.
    pub fn events(&self) -> Result<Vec<DiagnosticEvent>, KvpError>;

    /// Remove this layer's events (keys whose prefix+vm_id match).
    /// Leaves `Raw` records intact. Implemented as one
    /// [`KvpPoolStore::delete_many`] call so the entire clear happens
    /// under a single lock.
    pub fn clear(&self) -> Result<(), KvpError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticRecord {
    Event(DiagnosticEvent),
    Raw { key: String, value: String },
    Malformed { key: String, value: String, reason: String },
}
```

`records()` walks `KvpPoolStore::dump()` linearly, groups consecutive
entries with the same key into one `Event`, and falls through to `Raw`
or `Malformed` for keys that don't parse via `EventKey::parse`.
Reassembly is correct under concurrent writers because both `emit`
and `emit_raw` use `append_many`, which guarantees same-key chunks
are written contiguously on disk.

---

## 6. `KvpTracingLayer`

A `tracing_subscriber::Layer` that drives `DiagnosticsKvp`. This is the
replacement for `EmitKVPLayer` + `Kvp` in
[`libazureinit/src/kvp.rs`](../libazureinit/src/kvp.rs) and
[`libazureinit/src/logging.rs`](../libazureinit/src/logging.rs).

### 6.1 API

```rust
// src/diagnostics/tracing.rs   (feature = "tracing-layer")

pub struct KvpTracingLayer {
    diagnostics: DiagnosticsKvp,
}

impl KvpTracingLayer {
    pub fn new(diagnostics: DiagnosticsKvp) -> Self;
    pub fn diagnostics(&self) -> &DiagnosticsKvp;
}

impl<S> tracing_subscriber::Layer<S> for KvpTracingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{ ... }
```

Wrapping with a filter is the caller's job — same pattern used today at
[`libazureinit/src/logging.rs#L198`](../libazureinit/src/logging.rs#L198):

```rust
let layer = KvpTracingLayer::new(diagnostics).with_filter(env_filter);
```

### 6.2 Behavior (same semantics as legacy, simpler plumbing)

| Tracing event | Behavior |
|---|---|
| `on_new_span` | Store `std::time::Instant::now()` in span extensions. |
| `on_event` with a `health_report` field | `diagnostics.emit_raw("PROVISIONING_REPORT", value)`. Matches [`kvp.rs#L327`](../libazureinit/src/kvp.rs#L327). |
| `on_event` inside a span | Build a `DiagnosticEvent { level, name = format_span_name(span), event_id = Uuid::new_v4().to_string(), message = "Time: {ts} \| Event: {fields}", timestamp = now }` and call `diagnostics.emit(&event)`. |
| `on_close` | Build a `DiagnosticEvent` with `message = "Start: {start} \| End: {end}"`, emit. |

The value-string format (`"Time: ... | Event: ..."` vs.
`"Start: ... | End: ..."`) lives in the tracing layer because it's
the layer that knows the difference between an in-span event and a
span closure. `DiagnosticEvent.message` carries the literal bytes.

Helpers `format_span_name` and the field visitor are private to
`tracing.rs`. They move out of `libazureinit/src/kvp.rs` verbatim
during the wiring step.

### 6.3 What got simplified vs. legacy

| Legacy ([`kvp.rs`](../libazureinit/src/kvp.rs), [`logging.rs`](../libazureinit/src/logging.rs)) | New | Why |
|---|---|---|
| `Kvp` struct holds `Option<KvpLayer>`, `JoinHandle`, `CancellationToken`. `.layer()` panics on second call. | `KvpTracingLayer` is a plain struct; `Clone`-able if needed. | No background task → no handle, no shutdown signal, no Option/take dance. |
| `Kvp::new` spawns a tokio task ([`kvp.rs#L91`](../libazureinit/src/kvp.rs#L91)), creates an unbounded mpsc, opens a raw `File`, batches writes. | Constructor takes a `DiagnosticsKvp`. That's it. | `KvpPoolStore` already handles locking. The legacy batching was an optimization for a `File`+`fs2` writer that took one lock per batch; `append_many` gives us the same atomic-batch property in the new world, while single-chunk events use `append` (one lock, one record). |
| `kvp_writer` async task ([`kvp.rs#L120`](../libazureinit/src/kvp.rs#L120)) with select! loop, drain-on-cancel logic. | Gone. | No async runtime needed. |
| `MyInstant` wrapper ([`kvp.rs#L196`](../libazureinit/src/kvp.rs#L196)). | `std::time::Instant` directly. | Wrapper added nothing. |
| `encode_kvp_item` ([`kvp.rs#L455`](../libazureinit/src/kvp.rs#L455)) returns `Vec<Vec<u8>>` of zero-padded 2560-byte buffers, concatenated and written under one lock. | `chunk_at_char_boundary` returns `Vec<&str>`; `append_many` writes them under one lock and `KvpPoolStore` handles wire-format encoding. | Separation of concerns: chunking is content-level, encoding is wire-level, atomicity is the store's job. |
| `generate_event_key` ([`kvp.rs#L434`](../libazureinit/src/kvp.rs#L434)) free function. | `EventKey::format()` method on a struct. | Round-trips with `EventKey::parse()`; one place to change the format. |
| `truncate_guest_pool_file` ([`kvp.rs#L537`](../libazureinit/src/kvp.rs#L537)) — manual `/proc/stat` walk, mtime check, truncate. | `KvpPoolStore::clear_if_stale()`. | Already exists in the store. |
| `get_kvp_filter` ([`logging.rs#L58`](../libazureinit/src/logging.rs#L58)) — 60+ lines, three layers of fallback. | Stays in caller (azure-init binary) when wired. Optional: a `default_filter_string() -> &'static str` helper here. | Filter strings are application policy, not library policy. The complexity comes from azure-init's curated module list. |
| `EmitKVPLayer::handle_kvp_operation` flattens chunks with `.concat()` then sends through mpsc; writer task batches them. | `KvpTracingLayer::on_event` calls `diagnostics.emit(&event)` directly; chunking happens inside `emit` via `append_many`. | One synchronous path; nothing buffered, nothing dropped on cancel, no interleave hazard. |

### 6.4 Sync vs. async writes — tradeoff

`emit` runs synchronously inside the `tracing` callback. For azure-init
(low event frequency, short-lived process) this is fine: each emit is
one `flock` + one ~2.5 KB write + one unlock, dominated by syscall
latency well under a millisecond. There is no shared file handle, so
there is no contention beyond what `flock` arbitrates.

If a future high-frequency producer needs buffering, the right
mechanism is a thin wrapper in *that* application that pushes
`DiagnosticEvent`s through an mpsc to a worker thread. We do not
prebuild that here.

**Blocking note.** `KvpPoolStore::append_many` takes `flock(LOCK_EX)`,
which blocks indefinitely if another process (e.g. `hv_kvp_daemon`,
cloud-init) holds the lock. In practice these holds are sub-millisecond,
but a wedged peer wedges the tracing callback. Acceptable for
azure-init (short-lived process; a wedged daemon is already fatal);
documented so a future high-frequency caller knows what they're
signing up for.

**Failure mode.** If a write returns `Err`, the layer prints to
`stderr` via `eprintln!` and continues. We deliberately do **not**
use `tracing::warn!` here — that would re-enter the tracing
subscriber and, if the user's filter happens to match, recursively
invoke `on_event` while a write is failing. Matches legacy behavior
at [`kvp.rs#L172`](../libazureinit/src/kvp.rs#L172).

---

## 7. CLI binary (`azure-init-kvp`)

Lives at `src/bin/azure-init-kvp.rs`. The implementation lives in
`src/cli/` so it is testable as a library module.

```toml
[[bin]]
name = "azure-init-kvp"
required-features = ["cli"]
```

Build with `cargo build --release --features cli`. Library users who
don't want the binary pay nothing — `clap` etc. are not pulled in.

### 7.1 Top-level UX

```
azure-init-kvp [--pool {guest|external|auto|auto-external|auto-internal}]
               [--dir PATH]
               [--unsafe]
               <COMMAND>
```

Defaults: `--pool guest`, `--dir /var/lib/hyperv`, safe mode.

### 7.2 Raw subcommands (operate on `KvpPoolStore` directly)

| Command | Maps to |
|---|---|
| `dump [--json\|--ndjson\|--pretty]` | `KvpPoolStore::dump()` |
| `entries [--json]` | `KvpPoolStore::entries()` |
| `read <KEY>` | `KvpPoolStore::read(KEY)` |
| `write <KEY> <VALUE> [--append]` | `insert` (default) or `append` |
| `delete <KEY>` | `KvpPoolStore::delete(KEY)` |
| `clear [--if-stale] [--yes]` | `clear` / `clear_if_stale` |
| `info` | `pool()`, `path()`, `mode()`, `len()`, `is_stale()`, `max_*_size()` |

### 7.3 Diagnostics subcommands (operate on `DiagnosticsKvp`)

```
azure-init-kvp diag --vm-id <UUID> --event-prefix <STR> <SUBCOMMAND>
```

| Command | Behavior |
|---|---|
| `diag dump [--json\|--ndjson\|--pretty] [--include-raw]` | `DiagnosticsKvp::records()` |
| `diag events [--level LEVEL] [--name GLOB] [--since RFC3339]` | Filtered `DiagnosticsKvp::events()` |
| `diag tail [-n N=20]` | Last N events |
| `diag clear [--yes]` | `DiagnosticsKvp::clear()` |

### 7.4 JSON shape

Event-oriented, NDJSON by default for `dump`:

```json
{ "kind": "event",
  "level": "INFO",
  "name": "provision:user:create_user",
  "event_id": "8f3e9c4a-...",
  "timestamp": "2026-06-08T17:42:31.115Z",
  "message": "...",
  "chunks": 1 }
{ "kind": "raw", "key": "PROVISIONING_REPORT", "value": "..." }
{ "kind": "malformed", "key": "...", "value": "...", "reason": "..." }
```

Exit codes: `0` ok, `1` not found, `2` validation error, `3` I/O error.

---

## 8. Public surface (`lib.rs`)

```rust
mod error;
mod store;

#[cfg(feature = "diagnostics")]
pub mod diagnostics;

#[cfg(feature = "cli")]
pub mod cli;

pub use error::KvpError;
pub use store::{KvpPool, KvpPoolStore, PoolMode};

#[cfg(feature = "diagnostics")]
pub use diagnostics::{
    DiagnosticEvent, DiagnosticRecord, DiagnosticsKvp, EventKey,
    MAX_CHUNK_BYTES,
};

#[cfg(feature = "tracing-layer")]
pub use diagnostics::KvpTracingLayer;
```

---

## 9. Examples (in `examples/`)

- **`emit_event.rs`** — open a `KvpPoolStore::new_in(tempdir, Guest, Safe)`,
  wrap in `DiagnosticsKvp`, emit one short event and one long event,
  read everything back with `records()` and print as JSON. ~40 lines.
  Doctest source-of-truth for `DiagnosticsKvp::emit`.
- **`tracing_setup.rs`** — build a `Registry::default().with(KvpTracingLayer::new(...))`
  subscriber, run a couple of `#[instrument]`ed functions, then dump via
  `DiagnosticsKvp::records()`. Demonstrates the full end-to-end path
  and exercises chunking with a real `tracing` producer.

---

## 10. Test plan

Unit tests inside the crate:

- `chunk_at_char_boundary`: ASCII split, multi-byte UTF-8 boundary cases,
  empty input, input shorter than `max`.
- `EventKey::{format, parse}` round-trip; rejection of malformed keys.
- RFC3339 formatter: a few `SystemTime` → string fixtures.
- `KvpPoolStore::append_many`: empty input is a no-op; multi-record
  batch lands contiguously; validation failure aborts the whole batch
  with no records written; size of file == n * record_size.
- `KvpPoolStore::delete_many`: removes every matching record
  (including duplicates); unknown keys are silently skipped; returns
  correct count; empty input is a no-op.

Integration tests (`tests/`):

- `diagnostics_roundtrip.rs`: `emit` → `records` round-trip; short
  events, long events (`MAX_CHUNK_BYTES * 3 + 50`), mixed with
  `emit_raw` (including a `emit_raw` value that itself spans multiple
  chunks), with `Malformed` injected via direct `append`.
- `concurrent_emit.rs`: two threads each emit several multi-chunk
  events concurrently against the same pool; `records()` reassembles
  every event correctly (no interleaving). This is the regression
  gate for the `append_many` atomicity contract.
- `tracing_layer.rs`: build a subscriber, run instrumented functions
  (mirrors [`kvp.rs#L678`](../libazureinit/src/kvp.rs#L678)
  `test_emit_kvp_layer`), assert that `PROVISIONING_REPORT` records
  with `result=success` and `result=error` land in the pool, and that
  span Start/End events are emitted.
- `cli_smoke.rs`: invoke a few `clap` subcommands via `assert_cmd`
  against a temp pool, verify exit codes and JSON shape.

---

## 11. Migration plan (executed *after* this design lands)

This work intentionally leaves
[`libazureinit/src/kvp.rs`](../libazureinit/src/kvp.rs),
[`libazureinit/src/logging.rs`](../libazureinit/src/logging.rs), and
[`src/main.rs`](../src/main.rs) untouched. The crate gains capability;
azure-init's runtime path doesn't change until a follow-up.

Order of follow-up work:

1. **Store additions** (this PR): `KvpPoolStore::append_many` and
   `KvpPoolStore::delete_many`, with unit tests. No diagnostics code
   depends on these yet but they need to land first.
2. **Diagnostics primitives** (this PR): `chunk`, `key`, `event`,
   `DiagnosticsKvp`. No legacy changes.
3. **Tracing layer** (this PR): `KvpTracingLayer` behind
   `tracing-layer`. No legacy changes.
4. **CLI** (this PR): `cli` module + `src/bin/azure-init-kvp.rs`. No
   legacy changes.
5. **Examples + README** (this PR).
6. **Wiring PR (separate)**: rewrite [`src/main.rs`](../src/main.rs) to
   construct `KvpPoolStore::new(KvpPool::Guest, PoolMode::Safe)` →
   `DiagnosticsKvp` → `KvpTracingLayer`. Move the OTEL / stderr / file
   layers and filter-precedence logic out of
   [`libazureinit/src/logging.rs`](../libazureinit/src/logging.rs) and
   into the binary. Delete
   [`libazureinit/src/kvp.rs`](../libazureinit/src/kvp.rs),
   [`libazureinit/src/logging.rs`](../libazureinit/src/logging.rs), the
   `mod kvp;` line, and any now-unused deps in
   [`libazureinit/Cargo.toml`](../libazureinit/Cargo.toml).
7. **Cleanup PR (separate)**: remove `chrono`, `tokio-util`, `fs2`,
   etc. from `libazureinit/Cargo.toml` if nothing else uses them after
   step 6.

Tests preserved across the migration (in their new homes):

| Legacy test | Lands in |
|---|---|
| [`kvp.rs#L678`](../libazureinit/src/kvp.rs#L678) `test_emit_kvp_layer` | `libazureinit-kvp/tests/tracing_layer.rs` |
| [`kvp.rs#L810`](../libazureinit/src/kvp.rs#L810) `test_encode_kvp_item_value_length` | `chunk.rs` unit tests + `diagnostics_roundtrip.rs` |
| [`kvp.rs#L846`](../libazureinit/src/kvp.rs#L846) `test_emit_kvp_layer_disabled` | azure-init binary tests (step 6 PR) |
| [`logging.rs#L323+`](../libazureinit/src/logging.rs#L323) `test_kvp_filter_*` (4 tests) | azure-init binary tests (step 6 PR) |
| [`kvp.rs#L783`](../libazureinit/src/kvp.rs#L783) `test_truncate_guest_pool_file` | Already covered by existing `clear_if_stale` tests in `store.rs` |

---

## 12. Open questions

1. **Pipe (`|`) in `DiagnosticEvent::name`** — reject in
   `DiagnosticEvent::new`, or document only? Recommendation: reject.
   Keeps `EventKey::parse` unambiguous.
2. **`event_id` generation in `KvpTracingLayer`** — UUIDv4 matches
   legacy. Alternative would be a hash of `tracing::Id` (no `uuid`
   dep). UUIDv4 is gated behind `tracing-layer` so the core crate
   stays UUID-free; acceptable.
3. **Default event prefix** — the layer takes the prefix as a
   constructor argument. The azure-init binary will pass
   `format!("azure-init-{}", env!("CARGO_PKG_VERSION"))` at wiring
   time. No default in the library.
4. **Filter helper in the library** — do we want
   `pub fn default_filter_string() -> &'static str` here, or leave the
   curated list entirely in the azure-init binary? Recommendation:
   leave it in the binary. The list is application policy.
