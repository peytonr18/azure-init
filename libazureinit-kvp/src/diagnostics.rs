// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed diagnostics layer over the raw
//! [`KvpPoolStore`](crate::KvpPoolStore) key/value API.
//!
//! Where [`KvpPoolStore`](crate::KvpPoolStore) treats keys and values as
//! opaque bytes, [`DiagnosticsKvp`] understands the telemetry
//! conventions azure-init writes into the guest pool:
//!
//! - **Event keys** encode structured metadata as a six-segment,
//!   pipe-delimited string
//!   (`<prefix>|<boot_epoch_time>|<event_level>|<name>|<vm_id>|<event_id>`).
//! - **Chunking**: a single KVP record caps the value at the store's
//!   per-record limit (see [`MAX_CHUNK_BYTES`] for the safe-mode value).
//!   Longer messages are split at UTF-8 codepoint boundaries into
//!   multiple records written atomically under a single lock. Each chunk
//!   gets a unique key — the event key with a `|<subevent_index>` suffix
//!   (`<prefix>|<boot_epoch_time>|<event_level>|<name>|<vm_id>|<event_id>|<subevent_index>`),
//!   matching cloud-init's naming — so the Hyper-V host, which keeps only
//!   one record per key, retains every chunk. The chunks are regrouped
//!   into one event on read.
//! - **Classification**: [`records`](DiagnosticsKvp::records) sorts every
//!   stored record into a [`DiagnosticRecord`] — a reassembled
//!   [`DiagnosticEvent`], an unstructured [`Raw`](DiagnosticRecord::Raw)
//!   record such as `PROVISIONING_REPORT`, or a
//!   [`Malformed`](DiagnosticRecord::Malformed) event key.
//! - **cloud-init**: the reader also decodes cloud-init reporting entries
//!   (keys prefixed `CLOUD_INIT` with a JSON value) into
//!   [`CloudInitEvent`]s so the diagnostics CLI can display telemetry
//!   from either provisioning agent. This crate only *reads* that
//!   format; it does not write it.
//!
//! This module is policy only: all locking, size enforcement, and
//! on-disk encoding stay in [`KvpPoolStore`](crate::KvpPoolStore).
//!
//! # Example
//!
//! ```
//! use libazureinit_kvp::{
//!     DiagnosticsKvp, KvpPool, KvpPoolStore, PoolMode, MAX_CHUNK_BYTES,
//! };
//! use tracing::Level;
//!
//! # fn main() -> Result<(), libazureinit_kvp::KvpError> {
//! let dir = std::env::temp_dir()
//!     .join(format!("libazureinit-kvp-doc-{}", std::process::id()));
//! std::fs::create_dir_all(&dir)?;
//! let store = KvpPoolStore::new_in(KvpPool::Guest, &dir, PoolMode::Safe)?;
//! store.clear()?;
//!
//! let diagnostics =
//!     DiagnosticsKvp::new(store, "vm-1234", "azure-init-doc");
//!
//! // A short event lands in a single record.
//! diagnostics.emit(
//!     Level::INFO,
//!     "user:create_user",
//!     "Creating user azureuser",
//! )?;
//!
//! // A long message is split across records each with a unique
//! // `|<subevent_index>`-suffixed key, and reassembled on read.
//! let long = "x".repeat(MAX_CHUNK_BYTES * 2 + 10);
//! diagnostics.emit(Level::DEBUG, "config:dump", &long)?;
//!
//! let events = diagnostics.events()?;
//! assert_eq!(events.len(), 2);
//! assert_eq!(events[1].message.len(), MAX_CHUNK_BYTES * 2 + 10);
//!
//! # std::fs::remove_dir_all(&dir).ok();
//! # Ok(())
//! # }
//! ```

use serde::Deserialize;
use tracing::Level;
use uuid::Uuid;

use crate::{KvpError, KvpPoolStore};

/// Literal prefix identifying a cloud-init reporting KVP key.
const CLOUD_INIT_PREFIX: &str = "CLOUD_INIT";

/// Maximum number of value bytes per KVP record under
/// [`PoolMode::Safe`](crate::PoolMode::Safe).
///
/// This matches a safe-mode store's
/// [`KvpPoolStore::max_value_size`](crate::KvpPoolStore::max_value_size).
/// [`DiagnosticsKvp::emit`] splits messages longer than the store's
/// actual limit, so an [`Unsafe`](crate::PoolMode::Unsafe) store uses
/// its larger capacity; this constant is the conservative reference
/// value used throughout the diagnostics conventions.
pub const MAX_CHUNK_BYTES: usize = 1022;

/// Delimiter separating the segments of a diagnostic event key.
const EVENT_KEY_DELIMITER: char = '|';

/// Format a diagnostic event key as its `|`-delimited on-disk string:
/// `<prefix>|<boot_epoch_time>|<event_level>|<name>|<vm_id>|<event_id>`.
///
/// `boot_epoch_time` is the Unix epoch second the system booted (see
/// [`KvpPoolStore::boot_epoch`](crate::KvpPoolStore::boot_epoch)); it sits
/// in the same slot as cloud-init's incarnation so the two agents' keys
/// line up. The `event_level`/`name`/`vm_id` order likewise mirrors
/// cloud-init's `type`/`name`/`vm_id` layout. [`classify_key`] is the
/// inverse. For example:
///
/// ```text
/// azure-init-0.1.0|1785187982|INFO|user:create_user|3f2504e0-...|8f3e9c4a-...
/// ```
fn format_event_key(
    prefix: &str,
    boot_epoch_time: i64,
    event_level: Level,
    name: &str,
    vm_id: &str,
    event_id: &str,
) -> String {
    let d = EVENT_KEY_DELIMITER;
    format!(
        "{prefix}{d}{boot_epoch_time}{d}{event_level}{d}{name}{d}{vm_id}\
         {d}{event_id}"
    )
}

/// Outcome of inspecting a raw pool key.
enum KeyClass<'a> {
    /// The key is a well-formed azure-init event key.
    Event {
        boot_epoch_time: i64,
        event_level: Level,
        name: &'a str,
        vm_id: &'a str,
        event_id: &'a str,
    },
    /// The key is a well-formed cloud-init reporting event key
    /// (`CLOUD_INIT|<incarnation>|<type>|<name>|[<vm_id>|]<uuid>`).
    CloudInit {
        incarnation: &'a str,
        event_type: &'a str,
        name: &'a str,
        vm_id: Option<&'a str>,
        uuid: &'a str,
    },
    /// The key has the azure-init six-segment shape but is not a valid
    /// event (for example, an unrecognized level).
    Malformed { reason: String },
    /// The key is not an event key (e.g. `PROVISIONING_REPORT`).
    Raw,
}

/// Classify a raw pool key.
fn classify_key(key: &str) -> KeyClass<'_> {
    if key.split(EVENT_KEY_DELIMITER).next() == Some(CLOUD_INIT_PREFIX) {
        return classify_cloud_init_key(key);
    }

    // azure-init:
    // `<prefix>|<boot_epoch_time>|<event_level>|<name>|<vm_id>|<event_id>`
    let mut segments = key.split(EVENT_KEY_DELIMITER);

    // `str::split` always yields at least one element.
    let _prefix = segments.next();
    let (
        Some(boot_epoch),
        Some(event_level),
        Some(name),
        Some(vm_id),
        Some(event_id),
    ) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    )
    else {
        return KeyClass::Raw;
    };
    if segments.next().is_some() {
        // More than six segments: a `|` leaked into a field.
        return KeyClass::Raw;
    }

    // A non-numeric boot epoch means this is not an azure-init event key;
    // keep it as an opaque record rather than a malformed event.
    let Ok(boot_epoch_time) = boot_epoch.parse::<i64>() else {
        return KeyClass::Raw;
    };

    match event_level.parse::<Level>() {
        Ok(event_level) => KeyClass::Event {
            boot_epoch_time,
            event_level,
            name,
            vm_id,
            event_id,
        },
        Err(_) => KeyClass::Malformed {
            reason: format!("unrecognized level {event_level:?}"),
        },
    }
}

/// Classify a `CLOUD_INIT`-prefixed key into a [`KeyClass::CloudInit`].
///
/// Handles both the current layout
/// (`CLOUD_INIT|<incarnation>|<type>|<name>|<vm_id>|<uuid>`) and the
/// older one that predates the `vm_id` segment
/// (`CLOUD_INIT|<incarnation>|<type>|<name>|<uuid>`). Any other segment
/// count is treated as [`KeyClass::Raw`]. Parses by pulling segments
/// from the iterator so no intermediate collection is allocated.
fn classify_cloud_init_key(key: &str) -> KeyClass<'_> {
    let mut segments = key.split(EVENT_KEY_DELIMITER);
    // The caller matched the `CLOUD_INIT` prefix; skip it.
    let _prefix = segments.next();
    let (Some(incarnation), Some(event_type), Some(name), Some(fourth)) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return KeyClass::Raw;
    };
    let (vm_id, uuid) = match (segments.next(), segments.next()) {
        (None, None) => (None, fourth),
        (Some(uuid), None) => (Some(fourth), uuid),
        _ => return KeyClass::Raw,
    };
    KeyClass::CloudInit {
        incarnation,
        event_type,
        name,
        vm_id,
        uuid,
    }
}

/// Split `value` into pieces of at most `max_bytes` bytes each, always
/// at UTF-8 codepoint boundaries.
///
/// An empty input yields a single empty chunk so callers still write one
/// record. A codepoint wider than `max_bytes` (only possible for tiny
/// `max_bytes`, never for [`MAX_CHUNK_BYTES`]) is emitted whole so the
/// split always makes progress.
fn chunk_at_char_boundary(value: &str, max_bytes: usize) -> Vec<&str> {
    debug_assert!(max_bytes > 0, "max_bytes must be positive");
    if value.is_empty() {
        return vec![""];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < value.len() {
        if value.len() - start <= max_bytes {
            chunks.push(&value[start..]);
            break;
        }

        // Walk back from the byte limit to the nearest codepoint boundary.
        let mut end = start + max_bytes;
        while end > start && !value.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // One codepoint spans the whole window; take it whole.
            end = start + max_bytes + 1;
            while end < value.len() && !value.is_char_boundary(end) {
                end += 1;
            }
        }

        chunks.push(&value[start..end]);
        start = end;
    }
    chunks
}

/// Reject the `|` key delimiter in an event field so the formatted key
/// round-trips through [`classify_key`].
fn reject_delimiter(field: &'static str, value: &str) -> Result<(), KvpError> {
    if value.contains(EVENT_KEY_DELIMITER) {
        return Err(KvpError::EventFieldContainsDelimiter { field });
    }
    Ok(())
}

/// A single azure-init diagnostic event — the decoded form of one
/// azure-init KVP entry.
///
/// This is the crate's definition of what an azure-init KVP event looks
/// like: the boot-scoped `boot_epoch_time`/`vm_id`, the event-scoped
/// `event_level`/`name`/`event_id`, and the free-form `message`. Write one
/// with [`DiagnosticsKvp::emit`] (which stamps the boot-scoped fields from
/// the session and generates a fresh `event_id`); read events back, fully
/// populated, via [`records`](DiagnosticsKvp::records) /
/// [`events`](DiagnosticsKvp::events).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiagnosticEvent {
    /// Unix epoch second the system booted, shared by every event of one
    /// boot. Distinguishes this boot's telemetry from a previous boot's.
    pub boot_epoch_time: i64,
    /// Severity of the event.
    pub event_level: Level,
    /// Formatted event name, e.g. `user:create_user`.
    pub name: String,
    /// VM identifier the event was emitted under.
    pub vm_id: String,
    /// Per-emit identifier (UUIDv4); every chunk of one emitted event
    /// shares this value.
    pub event_id: String,
    /// Literal value bytes written to the pool. The diagnostics layer
    /// imposes no format on this string.
    pub message: String,
}

/// The JSON payload cloud-init stores as a reporting event's KVP value.
///
/// Only the fields the diagnostics reader surfaces are deserialized;
/// cloud-init also duplicates `name`/`type` here, but those are read
/// from the key. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct CloudInitValue {
    /// Human-readable message; defaults to empty when absent.
    #[serde(default)]
    msg: String,
    /// ISO-8601 timestamp.
    ts: Option<String>,
    /// Result string (e.g. `SUCCESS`), present on `finish` events.
    result: Option<String>,
    /// Duration in seconds, present on `finish` events.
    duration: Option<f64>,
}

/// A single cloud-init reporting event decoded from a KVP entry.
///
/// cloud-init encodes routing metadata in the key
/// (`CLOUD_INIT|<incarnation>|<type>|<name>|[<vm_id>|]<uuid>`) and the
/// event details as a JSON value; this type is the decoded union of the
/// two.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CloudInitEvent {
    /// Boot-time incarnation stamp from the key. cloud-init uses it to
    /// distinguish this boot's records from a previous boot's.
    pub incarnation: String,
    /// Provisioning phase from the key, e.g. `start` or `finish`.
    pub event_type: String,
    /// Event name from the key, e.g. `modules-final/config-scripts_user`.
    pub name: String,
    /// VM identifier from the key. Absent in cloud-init builds that
    /// predate the `vm_id` key segment.
    pub vm_id: Option<String>,
    /// Per-event UUID from the key.
    pub uuid: String,
    /// ISO-8601 timestamp from the JSON value, if present.
    pub timestamp: Option<String>,
    /// Result from the JSON value (e.g. `SUCCESS`), if present.
    pub result: Option<String>,
    /// Duration in seconds from the JSON value, if present.
    pub duration: Option<f64>,
    /// Human-readable message from the JSON value.
    pub message: String,
}

/// A single record read back from the pool and classified by
/// [`DiagnosticsKvp::records`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DiagnosticRecord {
    /// A reassembled azure-init diagnostic event.
    Event {
        /// The decoded event.
        event: DiagnosticEvent,
        /// Number of on-disk records the value spanned (1 when short).
        chunks: usize,
    },
    /// A decoded cloud-init reporting event.
    CloudInit {
        /// The decoded cloud-init event.
        event: CloudInitEvent,
        /// Number of on-disk records the value spanned (1 when short).
        chunks: usize,
    },
    /// An unstructured record whose key is not an event key, such as
    /// `PROVISIONING_REPORT`.
    Raw {
        /// The record key.
        key: String,
        /// The reassembled record value.
        value: String,
    },
    /// A record whose key is event-shaped but is not a valid event (for
    /// example, an unrecognized level or invalid cloud-init JSON).
    Malformed {
        /// The record key.
        key: String,
        /// The reassembled record value.
        value: String,
        /// Why the key failed to parse as an event.
        reason: String,
    },
}

/// A typed diagnostics view over a [`KvpPoolStore`].
///
/// Owns the event-key `prefix` and `vm_id` stamped into this layer's
/// events by [`emit`](Self::emit). See the
/// [module documentation](self) for the on-disk format.
#[derive(Clone, Debug)]
pub struct DiagnosticsKvp {
    store: KvpPoolStore,
    vm_id: String,
    event_prefix: String,
}

impl DiagnosticsKvp {
    /// Wrap `store` with the `vm_id` and `event_prefix` stamped into
    /// this layer's event keys.
    pub fn new(
        store: KvpPoolStore,
        vm_id: impl Into<String>,
        event_prefix: impl Into<String>,
    ) -> Self {
        Self {
            store,
            vm_id: vm_id.into(),
            event_prefix: event_prefix.into(),
        }
    }

    /// The underlying store.
    pub fn store(&self) -> &KvpPoolStore {
        &self.store
    }

    /// The VM identifier stamped into event keys.
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    /// The prefix stamped into event keys.
    pub fn event_prefix(&self) -> &str {
        &self.event_prefix
    }

    /// Emit an azure-init diagnostic event: format the key
    /// `<prefix>|<boot_epoch_time>|<event_level>|<name>|<vm_id>|<event_id>`
    /// and write `message` as its value. `boot_epoch_time` (from
    /// [`KvpPoolStore::boot_epoch`](crate::KvpPoolStore::boot_epoch)),
    /// `vm_id`, and `event_prefix` come from this layer; the `event_id` is
    /// a fresh UUIDv4.
    ///
    /// Messages longer than the store's per-record value limit are split
    /// at UTF-8 codepoint boundaries and written as multiple records
    /// atomically under a single lock via
    /// [`KvpPoolStore::append_multiple`]. Each chunk is keyed by the event
    /// key with a `|<subevent_index>` suffix so every record is unique —
    /// the Hyper-V host keeps only one record per key — and the chunks are
    /// regrouped by [`records`](Self::records) on read.
    ///
    /// Returns [`KvpError::EventFieldContainsDelimiter`] if the
    /// `event_prefix`, `vm_id`, or `name` contains the `|` key delimiter,
    /// which would make the key ambiguous to [`records`](Self::records).
    pub fn emit(
        &self,
        event_level: Level,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<(), KvpError> {
        let name = name.into();
        reject_delimiter("event_prefix", &self.event_prefix)?;
        reject_delimiter("vm_id", &self.vm_id)?;
        reject_delimiter("name", &name)?;

        let boot_epoch_time = self.store.boot_epoch()?;
        let event_id = Uuid::new_v4().to_string();
        let key = format_event_key(
            &self.event_prefix,
            boot_epoch_time,
            event_level,
            &name,
            &self.vm_id,
            &event_id,
        );

        self.write_chunked(&key, &message.into())
    }

    /// Split `value` at the store's per-record limit and append the
    /// chunks under `key` in one atomic batch.
    ///
    /// A single-record value keeps the bare event `key`. A value that
    /// spans multiple records gets one record per chunk, each keyed
    /// `<key>|<subevent_index>` (`0`, `1`, …) so no two records collide —
    /// the Hyper-V host keeps only one record per key. [`reassemble`]
    /// strips the subevent index to regroup the chunks on read.
    fn write_chunked(&self, key: &str, value: &str) -> Result<(), KvpError> {
        let chunks = chunk_at_char_boundary(value, self.store.max_value_size());
        if chunks.len() == 1 {
            return self
                .store
                .append_multiple(chunks.into_iter().map(|chunk| (key, chunk)));
        }
        let records: Vec<(String, &str)> = chunks
            .into_iter()
            .enumerate()
            .map(|(subevent_index, chunk)| {
                let chunk_key =
                    format!("{key}{EVENT_KEY_DELIMITER}{subevent_index}");
                (chunk_key, chunk)
            })
            .collect();
        self.store.append_multiple(records)
    }

    /// Read every record, reassembling chunked events and classifying
    /// each into a [`DiagnosticRecord`].
    ///
    /// Records are returned in on-disk order. Consecutive records that
    /// share an event key — ignoring the `|<subevent_index>` suffix — are
    /// one event; because [`emit`](Self::emit) writes an event's chunks
    /// contiguously under a single lock, reassembly is correct even under
    /// concurrent writers.
    pub fn records(&self) -> Result<Vec<DiagnosticRecord>, KvpError> {
        Ok(reassemble(self.store.dump()?))
    }

    /// Read back only the azure-init [`DiagnosticEvent`]s, in on-disk
    /// order.
    ///
    /// This deliberately excludes cloud-init events — which decode to the
    /// separate [`CloudInitEvent`] type — as well as raw and malformed
    /// records. Use [`records`](Self::records) for the full cross-agent
    /// view that includes cloud-init telemetry.
    pub fn events(&self) -> Result<Vec<DiagnosticEvent>, KvpError> {
        Ok(self
            .records()?
            .into_iter()
            .filter_map(|record| match record {
                DiagnosticRecord::Event { event, .. } => Some(event),
                DiagnosticRecord::CloudInit { .. }
                | DiagnosticRecord::Raw { .. }
                | DiagnosticRecord::Malformed { .. } => None,
            })
            .collect())
    }

    /// Remove every diagnostic key: any key that parses as an event key
    /// (including every `|<subevent_index>` chunk of a multi-record event)
    /// or a malformed event key. Raw records such as `PROVISIONING_REPORT`
    /// are left intact.
    pub fn clear(&self) -> Result<(), KvpError> {
        let keys: Vec<String> = self
            .store
            .dump()?
            .into_iter()
            .filter_map(|(key, _)| {
                let is_diagnostic = !matches!(
                    classify_key(base_event_key(&key)),
                    KeyClass::Raw
                );
                is_diagnostic.then_some(key)
            })
            .collect();
        self.store.delete_multiple(keys)?;
        Ok(())
    }
}

/// The event key a chunk belongs to.
///
/// [`DiagnosticsKvp::write_chunked`] gives each chunk of a multi-record
/// event a unique key by appending a `|<subevent_index>` (cloud-init's
/// term) to the event key, so the Hyper-V host — which keeps only one
/// record per key — retains every chunk. This returns the shared event
/// key used to regroup them on read: for a chunk key
/// `<event-key>|<subevent_index>` it strips the trailing index; any other
/// key (a single-record event, `PROVISIONING_REPORT`, a malformed key, …)
/// is returned unchanged.
fn base_event_key(key: &str) -> &str {
    split_subevent_index(key).0
}

/// Split a key into its base event key and optional trailing subevent
/// index. When the trailing segment is numeric and the base parses as an
/// event key — a valid azure-init or cloud-init event, or a malformed one
/// (event-shaped but with an unrecognized level) — returns
/// `(base, Some(index))`; any other key (a single-record event,
/// `PROVISIONING_REPORT`, …) returns `(key, None)`.
///
/// The subevent index is the same trailing `|<i>` cloud-init and
/// azure-init append to give each chunk a unique key; [`reassemble`] uses
/// it both to regroup an event's chunks and to restore their write order.
/// Malformed keys are included so a chunked malformed event still
/// regroups and is cleared consistently with a single-record one.
fn split_subevent_index(key: &str) -> (&str, Option<u32>) {
    if let Some((base, index)) = key.rsplit_once(EVENT_KEY_DELIMITER) {
        if let Ok(index) = index.parse::<u32>() {
            if matches!(
                classify_key(base),
                KeyClass::Event { .. }
                    | KeyClass::CloudInit { .. }
                    | KeyClass::Malformed { .. }
            ) {
                return (base, Some(index));
            }
        }
    }
    (key, None)
}

/// Group consecutive records sharing an event key — chunk
/// `|<subevent_index>` suffixes stripped — from [`KvpPoolStore::dump`]
/// and classify each group into a [`DiagnosticRecord`].
fn reassemble(dumped: Vec<(String, String)>) -> Vec<DiagnosticRecord> {
    let mut parsed = dumped
        .into_iter()
        .map(|(key, value)| {
            let (base, index) = split_subevent_index(&key);
            (base.to_string(), index, value)
        })
        .peekable();

    let mut records = Vec::new();
    while let Some((base, index, value)) = parsed.next() {
        let mut indexed = vec![(index, value)];
        while parsed.peek().is_some_and(|(next, _, _)| *next == base) {
            let (_, next_index, next_value) =
                parsed.next().expect("peeked value exists");
            indexed.push((next_index, next_value));
        }
        // Restore write order by subevent index. Stable, so a single
        // record (index `None`) or any equal indices keep on-disk order.
        indexed.sort_by_key(|(index, _)| *index);
        let chunk_values =
            indexed.into_iter().map(|(_, value)| value).collect();
        records.push(classify_record(base, chunk_values));
    }

    records
}

/// Turn one reassembled group of chunk values into a
/// [`DiagnosticRecord`].
///
/// `chunk_values` holds every record that shared the base event key,
/// already ordered by subevent index by [`reassemble`], and is never
/// empty. azure-init events and raw records concatenate their values
/// directly. cloud-init writes each chunk as a metadata object carrying a
/// slice of the escaped message, so those are stitched back together and
/// decoded (see [`decode_cloud_init_value`]).
fn classify_record(key: String, chunk_values: Vec<String>) -> DiagnosticRecord {
    let chunks = chunk_values.len();
    match classify_key(&key) {
        KeyClass::Event {
            boot_epoch_time,
            event_level,
            name,
            vm_id,
            event_id,
        } => DiagnosticRecord::Event {
            event: DiagnosticEvent {
                boot_epoch_time,
                event_level,
                name: name.to_string(),
                vm_id: vm_id.to_string(),
                event_id: event_id.to_string(),
                message: chunk_values.concat(),
            },
            chunks,
        },
        KeyClass::CloudInit {
            incarnation,
            event_type,
            name,
            vm_id,
            uuid,
        } => {
            // Own the key-derived fields up front so `key` and
            // `chunk_values` are free to move into a `Malformed` record
            // when a chunk's value fails to decode.
            let incarnation = incarnation.to_string();
            let event_type = event_type.to_string();
            let name = name.to_string();
            let vm_id = vm_id.map(str::to_string);
            let uuid = uuid.to_string();
            match decode_cloud_init_value(&chunk_values) {
                Ok(decoded) => DiagnosticRecord::CloudInit {
                    event: CloudInitEvent {
                        incarnation,
                        event_type,
                        name,
                        vm_id,
                        uuid,
                        timestamp: decoded.timestamp,
                        result: decoded.result,
                        duration: decoded.duration,
                        message: decoded.message,
                    },
                    chunks,
                },
                Err(err) => DiagnosticRecord::Malformed {
                    key,
                    value: chunk_values.concat(),
                    reason: format!("invalid cloud-init JSON value: {err}"),
                },
            }
        }
        KeyClass::Malformed { reason } => DiagnosticRecord::Malformed {
            key,
            value: chunk_values.concat(),
            reason,
        },
        KeyClass::Raw => DiagnosticRecord::Raw {
            key,
            value: chunk_values.concat(),
        },
    }
}

/// The fields [`classify_record`] pulls from a cloud-init event's
/// value(s): the reassembled `message` plus the metadata a `finish` event
/// carries.
struct DecodedCloudInit {
    timestamp: Option<String>,
    result: Option<String>,
    duration: Option<f64>,
    message: String,
}

/// Marker preceding a cloud-init value's message field: `"msg":"`.
const CLOUD_INIT_MSG_MARKER: &str = "\"msg\":\"";

/// Decode a cloud-init event's chunk value(s) into its metadata and full
/// message.
///
/// A single-record event is a complete JSON object, parsed directly. A
/// multi-record event was split by cloud-init's `_break_down`, which
/// slices the JSON-*escaped* message at character boundaries — so an
/// individual chunk can end mid-escape (e.g. a `\n` split into `\` and
/// `n`) and is not valid JSON on its own. We therefore recover the raw
/// (still-escaped) `msg` slice from each chunk, concatenate the slices in
/// order, and unescape the whole once; the metadata is read from the
/// first chunk's non-`msg` fields.
fn decode_cloud_init_value(
    chunks: &[String],
) -> Result<DecodedCloudInit, String> {
    if let [only] = chunks {
        let value: CloudInitValue =
            serde_json::from_str(only).map_err(|e| e.to_string())?;
        return Ok(DecodedCloudInit {
            timestamp: value.ts,
            result: value.result,
            duration: value.duration,
            message: value.msg,
        });
    }

    // Concatenate each chunk's raw escaped `msg` slice, then unescape the
    // reassembled string once so escapes split across chunks are rejoined
    // first.
    let mut escaped = String::new();
    for chunk in chunks {
        escaped.push_str(escaped_msg_slice(chunk)?);
    }
    let message: String = serde_json::from_str(&format!("\"{escaped}\""))
        .map_err(|e| e.to_string())?;

    // Metadata is identical across chunks; take it from the first, whose
    // non-`msg` prefix is always valid JSON.
    let meta = chunk_metadata(&chunks[0])?;
    Ok(DecodedCloudInit {
        timestamp: meta.ts,
        result: meta.result,
        duration: meta.duration,
        message,
    })
}

/// Recover a chunk's raw (still-escaped) `msg` slice — the bytes between
/// the `"msg":"` marker and the closing `"}` — without unescaping.
fn escaped_msg_slice(chunk: &str) -> Result<&str, String> {
    let start = chunk
        .find(CLOUD_INIT_MSG_MARKER)
        .ok_or("chunk is missing a \"msg\" field")?
        + CLOUD_INIT_MSG_MARKER.len();
    let end = chunk
        .strip_suffix("\"}")
        .map(str::len)
        .ok_or("chunk does not end with '\"}'")?;
    chunk
        .get(start..end)
        .ok_or_else(|| "chunk \"msg\" field is malformed".to_string())
}

/// Parse a chunk's non-`msg` metadata (`ts`/`result`/`duration`) from the
/// portion before its `,"msg":"` field. That prefix is always valid JSON
/// even when the trailing `msg` slice is not.
fn chunk_metadata(chunk: &str) -> Result<CloudInitValue, String> {
    let marker = format!(",{CLOUD_INIT_MSG_MARKER}");
    let end = chunk
        .find(&marker)
        .ok_or("chunk is missing a \"msg\" field")?;
    serde_json::from_str(&format!("{}}}", &chunk[..end]))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const PREFIX: &str = "azure-init-0.1.0";
    const VM_ID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const EVENT_ID: &str = "8f3e9c4a-1b2c-4d5e-9f01-234567890abc";
    const BOOT_EPOCH: i64 = 1_700_000_000;

    #[test]
    fn event_key_formats_and_classifies() {
        let formatted = format_event_key(
            PREFIX,
            BOOT_EPOCH,
            Level::INFO,
            "user:create_user",
            VM_ID,
            EVENT_ID,
        );
        assert_eq!(
            formatted,
            format!(
                "{PREFIX}|{BOOT_EPOCH}|INFO|user:create_user|{VM_ID}|\
                 {EVENT_ID}"
            )
        );
        assert!(matches!(
            classify_key(&formatted),
            KeyClass::Event {
                boot_epoch_time,
                event_level,
                name,
                vm_id,
                event_id,
            } if boot_epoch_time == BOOT_EPOCH
                && event_level == Level::INFO
                && name == "user:create_user"
                && vm_id == VM_ID
                && event_id == EVENT_ID
        ));
    }

    #[test]
    fn classify_round_trips_every_level() {
        for expected in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            let key = format_event_key(
                PREFIX,
                BOOT_EPOCH,
                expected,
                "span:event",
                VM_ID,
                EVENT_ID,
            );
            assert!(matches!(
                classify_key(&key),
                KeyClass::Event { event_level, .. } if event_level == expected
            ));
        }
    }

    /// Map a key to its [`KeyClass`] discriminant for table-driven tests.
    fn class_of(key: &str) -> &'static str {
        match classify_key(key) {
            KeyClass::Event { .. } => "event",
            KeyClass::CloudInit { .. } => "cloud-init",
            KeyClass::Malformed { .. } => "malformed",
            KeyClass::Raw => "raw",
        }
    }

    #[rstest]
    #[case::event("p|100|INFO|name|vm|id", "event")]
    #[case::cloud_init(
        "CLOUD_INIT|1785187982|finish|name|vmid|uuid",
        "cloud-init"
    )]
    #[case::raw_single_segment("PROVISIONING_REPORT", "raw")]
    #[case::raw_too_few_segments("a|b|INFO|c", "raw")]
    #[case::raw_too_many_segments("a|100|b|INFO|c|d|e", "raw")]
    #[case::raw_non_numeric_boot_epoch("p|notnum|INFO|name|vm|id", "raw")]
    #[case::malformed_bad_level("p|100|NOTALEVEL|name|vm|id", "malformed")]
    #[case::malformed_other_level("p|100|NOPE|name|vm|id", "malformed")]
    fn classify_key_categorizes(#[case] key: &str, #[case] expected: &str) {
        assert_eq!(class_of(key), expected);
    }

    #[rstest]
    #[case::empty("", 4, vec![""])]
    #[case::shorter_than_max("abc", 8, vec!["abc"])]
    #[case::exact_multiple("abcdef", 2, vec!["ab", "cd", "ef"])]
    #[case::ascii_remainder("abcde", 2, vec!["ab", "cd", "e"])]
    #[case::two_byte_boundary("aéb", 2, vec!["a", "é", "b"])]
    #[case::oversized_three_byte("€", 1, vec!["€"])]
    #[case::oversized_repeated("€€", 1, vec!["€", "€"])]
    fn chunk_splits_at_utf8_boundaries(
        #[case] input: &str,
        #[case] max_bytes: usize,
        #[case] expected: Vec<&str>,
    ) {
        assert_eq!(chunk_at_char_boundary(input, max_bytes), expected);
    }

    #[test]
    fn chunk_reassembles_multibyte_payload() {
        let payload = "🚀".repeat(100);
        let chunks = chunk_at_char_boundary(&payload, 7);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 7));
        assert_eq!(chunks.concat(), payload);
    }

    #[test]
    fn reject_delimiter_flags_pipe() {
        assert!(reject_delimiter("name", "no pipe here").is_ok());
        let err = reject_delimiter("name", "has|pipe").unwrap_err();
        assert!(matches!(
            err,
            KvpError::EventFieldContainsDelimiter { field: "name" }
        ));
        assert_eq!(
            err.to_string(),
            "event key field 'name' must not contain '|'"
        );
    }

    #[test]
    fn reassemble_groups_chunks_and_classifies() {
        let key = format_event_key(
            PREFIX,
            BOOT_EPOCH,
            Level::INFO,
            "config:dump",
            VM_ID,
            EVENT_ID,
        );

        let dumped = vec![
            (key.clone(), "part-one/".to_string()),
            (key.clone(), "part-two".to_string()),
            (
                "PROVISIONING_REPORT".to_string(),
                "result=success".to_string(),
            ),
            ("p|100|NOPE|name|vm|id".to_string(), "junk".to_string()),
        ];

        let records = reassemble(dumped);
        assert_eq!(records.len(), 3);

        assert_eq!(
            records[0],
            DiagnosticRecord::Event {
                event: DiagnosticEvent {
                    boot_epoch_time: BOOT_EPOCH,
                    event_level: Level::INFO,
                    name: "config:dump".to_string(),
                    vm_id: VM_ID.to_string(),
                    event_id: EVENT_ID.to_string(),
                    message: "part-one/part-two".to_string(),
                },
                chunks: 2,
            }
        );
        assert!(matches!(&records[1], DiagnosticRecord::Raw { key, .. }
            if key == "PROVISIONING_REPORT"));
        assert!(matches!(&records[2], DiagnosticRecord::Malformed { .. }));
    }

    #[test]
    fn reassemble_keeps_distinct_adjacent_keys_separate() {
        let make = |event_id: &str| {
            format_event_key(
                PREFIX,
                BOOT_EPOCH,
                Level::INFO,
                "span:name",
                VM_ID,
                event_id,
            )
        };
        let dumped = vec![
            (make("id-1"), "first".to_string()),
            (make("id-2"), "second".to_string()),
        ];
        let records = reassemble(dumped);
        assert_eq!(records.len(), 2);
        assert!(matches!(
            &records[0],
            DiagnosticRecord::Event { chunks: 1, .. }
        ));
        assert!(matches!(
            &records[1],
            DiagnosticRecord::Event { chunks: 1, .. }
        ));
    }

    #[rstest]
    #[case::indexed_chunk("p|100|INFO|name|vm|id|0", "p|100|INFO|name|vm|id")]
    #[case::indexed_chunk_multi_digit(
        "p|100|INFO|name|vm|id|12",
        "p|100|INFO|name|vm|id"
    )]
    #[case::single_event_unchanged(
        "p|100|INFO|name|vm|id",
        "p|100|INFO|name|vm|id"
    )]
    #[case::cloud_init_indexed_chunk(
        "CLOUD_INIT|1785187982|finish|mod|vmid|uuid|0",
        "CLOUD_INIT|1785187982|finish|mod|vmid|uuid"
    )]
    #[case::raw_unchanged("PROVISIONING_REPORT", "PROVISIONING_REPORT")]
    #[case::non_event_numeric_tail_unchanged("foo|3", "foo|3")]
    #[case::malformed_unchanged(
        "p|100|NOPE|name|vm|id",
        "p|100|NOPE|name|vm|id"
    )]
    #[case::malformed_indexed_chunk(
        "p|100|NOPE|name|vm|id|0",
        "p|100|NOPE|name|vm|id"
    )]
    fn base_event_key_strips_event_subevent_index(
        #[case] key: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(base_event_key(key), expected);
    }

    #[test]
    fn reassemble_groups_indexed_chunk_keys() {
        let base = format_event_key(
            PREFIX,
            BOOT_EPOCH,
            Level::INFO,
            "config:dump",
            VM_ID,
            EVENT_ID,
        );
        let dumped = vec![
            (format!("{base}|0"), "part-one/".to_string()),
            (format!("{base}|1"), "part-two/".to_string()),
            (format!("{base}|2"), "part-three".to_string()),
        ];

        let records = reassemble(dumped);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0],
            DiagnosticRecord::Event {
                event: DiagnosticEvent {
                    boot_epoch_time: BOOT_EPOCH,
                    event_level: Level::INFO,
                    name: "config:dump".to_string(),
                    vm_id: VM_ID.to_string(),
                    event_id: EVENT_ID.to_string(),
                    message: "part-one/part-two/part-three".to_string(),
                },
                chunks: 3,
            }
        );
    }

    // ---- cloud-init read support ----

    const CLOUD_INIT_VM_ID: &str = "0e5e179d-5341-478b-8456-fbb90621bdf8";
    const CLOUD_INIT_KEY_FINISH: &str = "CLOUD_INIT|1785187982|finish|modules-final/config-scripts_user|0e5e179d-5341-478b-8456-fbb90621bdf8|e5f01809-a7a3-4279-aa64-1f18e21eda6e";
    const CLOUD_INIT_VALUE_FINISH: &str = r#"{"name":"modules-final/config-scripts_user","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.0006448590000012189,"msg":"config-scripts_user ran successfully and took 0.001 seconds"}"#;

    #[rstest]
    #[case::with_vm_id(
        CLOUD_INIT_KEY_FINISH,
        "1785187982",
        "finish",
        "modules-final/config-scripts_user",
        Some(CLOUD_INIT_VM_ID),
        "e5f01809-a7a3-4279-aa64-1f18e21eda6e"
    )]
    // Older cloud-init builds omit the vm_id key segment.
    #[case::without_vm_id(
        "CLOUD_INIT|1785187982|start|modules-config/foo|c4d4a08d-fe93-4c7a-9be6-9a38c212e212",
        "1785187982",
        "start",
        "modules-config/foo",
        None,
        "c4d4a08d-fe93-4c7a-9be6-9a38c212e212"
    )]
    fn cloud_init_key_classifies(
        #[case] key: &str,
        #[case] incarnation: &str,
        #[case] event_type: &str,
        #[case] name: &str,
        #[case] vm_id: Option<&str>,
        #[case] uuid: &str,
    ) {
        assert!(matches!(
            classify_key(key),
            KeyClass::CloudInit {
                incarnation: i,
                event_type: t,
                name: n,
                vm_id: v,
                uuid: u,
            } if i == incarnation
                && t == event_type
                && n == name
                && v == vm_id
                && u == uuid
        ));
    }

    #[rstest]
    #[case::too_many("CLOUD_INIT|a|b|c|d|e|f", "raw")]
    #[case::too_few("CLOUD_INIT|a|b|c", "raw")]
    #[case::prefix_only("CLOUD_INIT", "raw")]
    fn cloud_init_bad_shapes_are_raw(
        #[case] key: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(class_of(key), expected);
    }

    #[test]
    fn cloud_init_finish_record_decodes_all_fields() {
        // A single `matches!` covers every field (with a tolerance on the
        // float duration) and leaves no unreachable arm to cover.
        assert!(matches!(
            classify_record(
                CLOUD_INIT_KEY_FINISH.to_string(),
                vec![CLOUD_INIT_VALUE_FINISH.to_string()],
            ),
            DiagnosticRecord::CloudInit { event, chunks: 1 }
            if event.incarnation == "1785187982"
                && event.event_type == "finish"
                && event.name == "modules-final/config-scripts_user"
                && event.vm_id.as_deref() == Some(CLOUD_INIT_VM_ID)
                && event.uuid == "e5f01809-a7a3-4279-aa64-1f18e21eda6e"
                && event.timestamp.as_deref()
                    == Some("2026-07-27T21:33:24.339006+00:00")
                && event.result.as_deref() == Some("SUCCESS")
                && event.duration.is_some_and(|d| {
                    (d - 0.000_644_859_000_001_218_9).abs() < 1e-12
                })
                && event.message
                    == "config-scripts_user ran successfully and took \
                        0.001 seconds"
        ));
    }

    #[test]
    fn cloud_init_start_record_has_no_result_or_duration() {
        let value = r#"{"name":"modules-final/config-keys_to_console","type":"start","ts":"2026-07-27T21:33:24.344349+00:00","msg":"running config-keys_to_console with frequency once-per-instance"}"#;
        let key = "CLOUD_INIT|1785187982|start|modules-final/config-keys_to_console|0e5e179d-5341-478b-8456-fbb90621bdf8|7792621b-b339-4274-8b71-2a3dcbd2db4e";
        assert_eq!(
            classify_record(key.to_string(), vec![value.to_string()]),
            DiagnosticRecord::CloudInit {
                event: CloudInitEvent {
                    incarnation: "1785187982".to_string(),
                    event_type: "start".to_string(),
                    name: "modules-final/config-keys_to_console".to_string(),
                    vm_id: Some(CLOUD_INIT_VM_ID.to_string()),
                    uuid: "7792621b-b339-4274-8b71-2a3dcbd2db4e".to_string(),
                    timestamp: Some(
                        "2026-07-27T21:33:24.344349+00:00".to_string()
                    ),
                    result: None,
                    duration: None,
                    message: "running config-keys_to_console with frequency \
                               once-per-instance"
                        .to_string(),
                },
                chunks: 1,
            }
        );
    }

    #[test]
    fn cloud_init_key_with_invalid_json_is_malformed() {
        assert!(matches!(
            classify_record(
                CLOUD_INIT_KEY_FINISH.to_string(),
                vec!["not json".to_string()],
            ),
            DiagnosticRecord::Malformed { reason, .. }
            if reason.contains("cloud-init")
        ));
    }

    #[test]
    fn cloud_init_chunks_reassemble_by_subevent_index() {
        let base = "CLOUD_INIT|1785187982|finish|modules-final/long|0e5e179d-5341-478b-8456-fbb90621bdf8|abc12345-1111-2222-3333-444455556666";
        // Each chunk carries its own `|<subevent_index>` key suffix and a
        // message slice. They are laid out on disk out of order to prove
        // reassembly restores order by that index, not disk position.
        let chunk = |i: u32, msg: &str| {
            format!(
                r#"{{"name":"modules-final/long","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.5,"msg_i":{i},"msg":"{msg}"}}"#
            )
        };
        let dumped = vec![
            (format!("{base}|1"), chunk(1, "two ")),
            (format!("{base}|0"), chunk(0, "one ")),
            (format!("{base}|2"), chunk(2, "three")),
        ];

        let records = reassemble(dumped);
        assert_eq!(
            records,
            vec![DiagnosticRecord::CloudInit {
                event: CloudInitEvent {
                    incarnation: "1785187982".to_string(),
                    event_type: "finish".to_string(),
                    name: "modules-final/long".to_string(),
                    vm_id: Some(CLOUD_INIT_VM_ID.to_string()),
                    uuid: "abc12345-1111-2222-3333-444455556666".to_string(),
                    timestamp: Some(
                        "2026-07-27T21:33:24.339006+00:00".to_string()
                    ),
                    result: Some("SUCCESS".to_string()),
                    duration: Some(0.5),
                    message: "one two three".to_string(),
                },
                chunks: 3,
            }]
        );
    }

    #[test]
    fn cloud_init_chunks_reassemble_split_json_escape() {
        // Regression: cloud-init's `_break_down` re-emits the metadata
        // (plus a `msg_i` chunk index) on every chunk and slices the
        // JSON-escaped message at character boundaries, so a `\n` escape
        // can straddle two chunks — the first ends in a lone backslash and
        // is not valid JSON on its own. The reader must rejoin the raw
        // slices before unescaping.
        let base = "CLOUD_INIT|1785187982|finish|modules-final/x|0e5e179d-5341-478b-8456-fbb90621bdf8|abc12345-1111-2222-3333-444455556666";
        let dumped = vec![
            (
                format!("{base}|0"),
                r#"{"name":"modules-final/x","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.5,"msg_i":0,"msg":"line1\"}"#
                    .to_string(),
            ),
            (
                format!("{base}|1"),
                r#"{"name":"modules-final/x","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.5,"msg_i":1,"msg":"nline2"}"#
                    .to_string(),
            ),
        ];

        let records = reassemble(dumped);
        assert!(matches!(
            &records[..],
            [DiagnosticRecord::CloudInit { event, chunks: 2 }]
            if event.message == "line1\nline2"
                && event.result.as_deref() == Some("SUCCESS")
                && event.duration == Some(0.5)
        ));
    }
}
