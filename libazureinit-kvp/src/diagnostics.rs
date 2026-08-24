// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed diagnostics layer over the raw
//! [`KvpPoolStore`](crate::KvpPoolStore) key/value API.
//!
//! Where [`KvpPoolStore`](crate::KvpPoolStore) treats keys and values as
//! opaque bytes, [`DiagnosticsKvp`] understands the telemetry conventions
//! azure-init writes into the guest pool and decodes cloud-init's
//! reporting entries into the same [`DiagnosticEvent`] shape.
//!
//! - **azure-init keys** encode metadata as a seven-segment,
//!   pipe-delimited string
//!   (`<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>`).
//!   The value is the record's message string, stored verbatim for
//!   every kind.
//! - **cloud-init keys**
//!   (`CLOUD_INIT|<incarnation>|<type>|<name>|[<vm_id>|]<uuid>`) store a
//!   JSON value; the reader pulls `ts`/`result`/`duration`/`msg` from it
//!   and takes everything else from the key, decoding into the same
//!   [`DiagnosticEvent`]. This crate only *reads* cloud-init.
//! - **Chunking**: values longer than [`MAX_CHUNK_BYTES`] are split at
//!   UTF-8 codepoint boundaries into multiple records under one lock,
//!   each keyed with a unique `|<subevent_index>` suffix (`0`, `1`, …)
//!   since the Hyper-V host keeps only one record per key. Chunks are
//!   regrouped on read.
//! - **Classification**: [`records`](DiagnosticsKvp::records) sorts every
//!   stored record into a [`DiagnosticRecord`] — a reassembled
//!   [`DiagnosticEvent`] (from either agent), an unstructured
//!   [`Raw`](DiagnosticRecord::Raw) record such as `PROVISIONING_REPORT`,
//!   or a [`Malformed`](DiagnosticRecord::Malformed) event key.
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
//! diagnostics.emit_event("user:create_user", "Creating user azureuser")?;
//!
//! // A long message is split across records and reassembled on read.
//! let long = "x".repeat(MAX_CHUNK_BYTES * 2 + 10);
//! diagnostics.emit_event("config:dump", &long)?;
//!
//! let events = diagnostics.events()?;
//! assert_eq!(events.len(), 2);
//! assert_eq!(events[1].message.len(), MAX_CHUNK_BYTES * 2 + 10);
//!
//! # std::fs::remove_dir_all(&dir).ok();
//! # Ok(())
//! # }
//! ```

use chrono::Utc;
use uuid::Uuid;

use crate::{KvpError, KvpPoolStore};

/// Literal prefix identifying a cloud-init reporting KVP key.
const CLOUD_INIT_PREFIX: &str = "CLOUD_INIT";

/// Maximum number of value bytes per diagnostic KVP record.
///
/// [`DiagnosticsKvp::emit_event`] splits messages longer than this into
/// multiple records, regardless of the store's
/// [`PoolMode`](crate::PoolMode). It is the conservative
/// [`Safe`](crate::PoolMode::Safe) limit (2 bytes under the Linux kernel
/// `HV_KVP_EXCHANGE_MAX_VALUE` maximum), so diagnostic records stay
/// readable by the Hyper-V host even on an
/// [`Unsafe`](crate::PoolMode::Unsafe) store — its larger capacity is
/// deliberately not used for diagnostics.
pub const MAX_CHUNK_BYTES: usize = 1022;

/// Delimiter separating the segments of a diagnostic event key.
const EVENT_KEY_DELIMITER: char = '|';

/// The kind of a diagnostic record. `start`/`finish`/`event` are shared
/// with azure-init; cloud-init's other reporting types (`diagnostic`,
/// `compressed`, `boot-telemetry`, …) are kept verbatim as
/// [`Other`](RecordKind::Other).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordKind {
    /// A span opening, written `start`.
    Start,
    /// A span closing, written `finish`.
    Finish,
    /// A point-in-time event, written `event`.
    Event,
    /// Any other reporting type, kept verbatim (cloud-init only;
    /// azure-init never writes it).
    Other(String),
}

impl std::fmt::Display for RecordKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Start => "start",
            Self::Finish => "finish",
            Self::Event => "event",
            Self::Other(token) => token.as_str(),
        })
    }
}

/// Serializes as the on-disk token (the `Display` form).
impl serde::Serialize for RecordKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Parses an azure-init `kind` token — strictly `start`/`finish`/`event`
/// (a corrupt azure-init kind is rejected). cloud-init's wider `type`
/// space is mapped separately and falls back to [`RecordKind::Other`].
impl std::str::FromStr for RecordKind {
    type Err = ();

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        match token {
            "start" => Ok(Self::Start),
            "finish" => Ok(Self::Finish),
            "event" => Ok(Self::Event),
            _ => Err(()),
        }
    }
}

/// The current time as an ISO-8601 UTC timestamp (millisecond precision).
fn now_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Format an azure-init diagnostic event's *shared* key as its
/// `|`-delimited on-disk string:
/// `<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>`.
///
/// `boot_epoch` is the Unix epoch second the system booted (see
/// [`KvpPoolStore::boot_epoch`](crate::KvpPoolStore::boot_epoch)); it sits
/// in the same slot as cloud-init's incarnation. `kind` records the
/// span/event shape. [`classify_key`] is the inverse. For example:
///
/// ```text
/// azure-init-0.1.0|1785187982|3f2504e0-...|event|user:create_user|8f3e9c4a-...|2026-07-27T21:33:24.300Z
/// ```
fn format_event_key(
    agent: &str,
    boot_epoch: i64,
    vm_id: &str,
    kind: RecordKind,
    name: &str,
    event_id: &str,
    timestamp: &str,
) -> String {
    let d = EVENT_KEY_DELIMITER;
    format!(
        "{agent}{d}{boot_epoch}{d}{vm_id}{d}{kind}{d}{name}{d}{event_id}\
         {d}{timestamp}"
    )
}
enum KeyClass<'a> {
    Event {
        agent: &'a str,
        boot_epoch: i64,
        vm_id: &'a str,
        kind: RecordKind,
        name: &'a str,
        event_id: &'a str,
        timestamp: &'a str,
    },
    /// The key is a well-formed cloud-init reporting event key
    /// (`CLOUD_INIT|<incarnation>|<type>|<name>|[<vm_id>|]<uuid>`).
    CloudInit {
        boot_epoch: i64,
        kind: RecordKind,
        name: &'a str,
        vm_id: Option<&'a str>,
        uuid: &'a str,
    },
    Malformed {
        reason: String,
    },
    Raw,
}

/// Classify a raw pool key.
fn classify_key(key: &str) -> KeyClass<'_> {
    if key.split(EVENT_KEY_DELIMITER).next() == Some(CLOUD_INIT_PREFIX) {
        return classify_cloud_init_key(key);
    }

    let mut segments = key.split(EVENT_KEY_DELIMITER);
    let (
        Some(agent),
        Some(boot_epoch),
        Some(vm_id),
        Some(kind),
        Some(name),
        Some(event_id),
        Some(timestamp),
    ) = (
        segments.next(),
        segments.next(),
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
        return KeyClass::Raw;
    }

    let Ok(boot_epoch) = boot_epoch.parse::<i64>() else {
        return KeyClass::Raw;
    };

    match kind.parse::<RecordKind>() {
        Ok(kind) => KeyClass::Event {
            agent,
            boot_epoch,
            vm_id,
            kind,
            name,
            event_id,
            timestamp,
        },
        Err(()) => KeyClass::Malformed {
            reason: format!("unrecognized kind {kind:?}"),
        },
    }
}

/// Classify a `CLOUD_INIT`-prefixed key into a [`KeyClass::CloudInit`].
///
/// Handles the current layout
/// (`CLOUD_INIT|<incarnation>|<type>|<name>|<vm_id>|<uuid>`) and the
/// older one without the `vm_id` segment. A wrong segment count is
/// [`KeyClass::Raw`]; a non-numeric incarnation is
/// [`KeyClass::Malformed`]. Any `type` other than `start`/`finish`/`event`
/// is preserved as [`RecordKind::Other`], not rejected.
fn classify_cloud_init_key(key: &str) -> KeyClass<'_> {
    let mut segments = key.split(EVENT_KEY_DELIMITER);
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
    let Ok(boot_epoch) = incarnation.parse::<i64>() else {
        return KeyClass::Malformed {
            reason: format!(
                "non-numeric cloud-init incarnation {incarnation:?}"
            ),
        };
    };
    // Non-span cloud-init types are kept verbatim, not rejected.
    let kind = event_type
        .parse::<RecordKind>()
        .unwrap_or_else(|()| RecordKind::Other(event_type.to_string()));
    KeyClass::CloudInit {
        boot_epoch,
        kind,
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

        let mut end = start + max_bytes;
        while end > start && !value.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
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

/// A single diagnostic event — the decoded, source-agnostic form of one
/// azure-init or cloud-init KVP entry.
///
/// Metadata (`agent`, `boot_epoch`, `vm_id`, `kind`, `name`, `event_id`)
/// comes from the record key; the payload (`timestamp`, `result`,
/// `duration`, `message`) from the value. Optional fields are populated
/// only when the source provides them (e.g. cloud-init `finish` records
/// carry `result` and `duration`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub struct DiagnosticEvent {
    /// Reporting agent identifier from the key, e.g. `azure-init-0.1.0`
    /// or `CLOUD_INIT`; also distinguishes the record's source.
    pub agent: String,
    /// Unix epoch second the system booted (cloud-init's incarnation),
    /// shared by every record of one boot.
    pub boot_epoch: i64,
    /// VM identifier from the key. Absent in cloud-init builds that
    /// predate the `vm_id` key segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<String>,
    /// Whether this record opens a span, closes a span, or is a point
    /// event.
    pub kind: RecordKind,
    /// Formatted event or span name, e.g. `user:create_user`.
    pub name: String,
    /// Per-record identifier (azure-init's UUIDv4 / cloud-init's uuid);
    /// every chunk of one record shares it, as do a span's start and
    /// finish.
    pub event_id: String,
    /// ISO-8601 timestamp, if the source provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Result string (e.g. `SUCCESS`), present on cloud-init `finish`
    /// records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Duration in seconds, present on cloud-init `finish` records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Human-readable message. The diagnostics layer imposes no format
    /// on this string.
    pub message: String,
}

/// A single record read back from the pool and classified by
/// [`DiagnosticsKvp::records`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DiagnosticRecord {
    /// A reassembled diagnostic event, from either agent.
    Decoded {
        /// The decoded event.
        event: DiagnosticEvent,
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
    /// example, an unrecognized kind or invalid cloud-init JSON).
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
/// Owns the `agent` and `vm_id` stamped into this layer's azure-init
/// event keys. See the module-level documentation for the on-disk
/// format.
#[derive(Clone, Debug)]
pub struct DiagnosticsKvp {
    store: KvpPoolStore,
    vm_id: String,
    agent: String,
}

impl DiagnosticsKvp {
    pub fn new(
        store: KvpPoolStore,
        vm_id: impl Into<String>,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            store,
            vm_id: vm_id.into(),
            agent: agent.into(),
        }
    }
    pub fn store(&self) -> &KvpPoolStore {
        &self.store
    }
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Emit an azure-init point event with `name` and `message`.
    ///
    /// The `message` is stored verbatim as the record value. A fresh
    /// `event_id` (UUIDv4) is generated.
    pub fn emit_event(
        &self,
        name: impl Into<String>,
        message: impl AsRef<str>,
    ) -> Result<(), KvpError> {
        let event_id = Uuid::new_v4().to_string();
        self.write_event(
            RecordKind::Event,
            &event_id,
            &name.into(),
            message.as_ref(),
        )
    }

    /// Format the key
    /// `<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>`
    /// (stamping `boot_epoch`, `vm_id`, `agent`, and the current
    /// `timestamp`) and write `value` as its message.
    ///
    /// The message is written under a single lock via
    /// [`KvpPoolStore::append_multiple`]; values longer than
    /// [`MAX_CHUNK_BYTES`] are split at UTF-8 codepoint boundaries into
    /// multiple records. Every record is keyed with a `|<subevent_index>`
    /// suffix (`0`, `1`, …) so it is unique, and the chunks are regrouped
    /// by [`records`](Self::records) on read.
    ///
    /// Returns [`KvpError::EventFieldContainsDelimiter`] if `agent`,
    /// `vm_id`, `name`, or `event_id` contains the `|` key delimiter.
    fn write_event(
        &self,
        kind: RecordKind,
        event_id: &str,
        name: &str,
        value: &str,
    ) -> Result<(), KvpError> {
        reject_delimiter("agent", &self.agent)?;
        reject_delimiter("vm_id", &self.vm_id)?;
        reject_delimiter("name", name)?;
        reject_delimiter("event_id", event_id)?;

        let boot_epoch = self.store.boot_epoch()?;
        let timestamp = now_timestamp();
        let key = format_event_key(
            &self.agent,
            boot_epoch,
            &self.vm_id,
            kind,
            name,
            event_id,
            &timestamp,
        );

        self.write_chunked(&key, value)
    }

    /// Split `value` at [`MAX_CHUNK_BYTES`] and append each chunk in one
    /// atomic batch, keyed `<key>|<subevent_index>` (`0`, `1`, …) so every
    /// record is unique. [`reassemble`] strips the index on read.
    fn write_chunked(&self, key: &str, value: &str) -> Result<(), KvpError> {
        let records: Vec<(String, &str)> =
            chunk_at_char_boundary(value, MAX_CHUNK_BYTES)
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
    /// one event; because [`emit_event`](Self::emit_event) writes an
    /// event's chunks contiguously under a single lock, reassembly is
    /// correct even under concurrent writers.
    pub fn records(&self) -> Result<Vec<DiagnosticRecord>, KvpError> {
        Ok(reassemble(self.store.dump()?))
    }

    /// Read back every decoded [`DiagnosticEvent`], from either agent, in
    /// on-disk order.
    ///
    /// Raw and malformed records are excluded. Use
    /// [`records`](Self::records) for the full view that includes them.
    pub fn events(&self) -> Result<Vec<DiagnosticEvent>, KvpError> {
        Ok(self
            .records()?
            .into_iter()
            .filter_map(|record| match record {
                DiagnosticRecord::Decoded { event, .. } => Some(event),
                DiagnosticRecord::Raw { .. }
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

/// The shared event key a chunk belongs to: strips a trailing
/// `|<subevent_index>`, or returns the key unchanged if it has none.
fn base_event_key(key: &str) -> &str {
    split_subevent_index(key).0
}

/// Split a key into its base event key and optional trailing subevent
/// index: `(base, Some(index))` when a trailing numeric segment follows an
/// event-shaped base (valid or malformed), else `(key, None)`.
/// [`reassemble`] uses the index to regroup an event's chunks and restore
/// their write order.
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

/// Classify one reassembled group of chunk values (ordered by subevent
/// index, never empty) into a [`DiagnosticRecord`]. azure-init and raw
/// records concatenate their values; cloud-init chunks are stitched and
/// decoded via [`decode_cloud_init_value`].
fn classify_record(key: String, chunk_values: Vec<String>) -> DiagnosticRecord {
    let chunks = chunk_values.len();
    match classify_key(&key) {
        KeyClass::Event {
            agent,
            boot_epoch,
            vm_id,
            kind,
            name,
            event_id,
            timestamp,
        } => {
            let message = chunk_values.concat();
            DiagnosticRecord::Decoded {
                event: DiagnosticEvent {
                    agent: agent.to_string(),
                    boot_epoch,
                    vm_id: Some(vm_id.to_string()),
                    kind,
                    name: name.to_string(),
                    event_id: event_id.to_string(),
                    timestamp: Some(timestamp.to_string()),
                    result: None,
                    duration: None,
                    message,
                },
                chunks,
            }
        }
        KeyClass::CloudInit {
            boot_epoch,
            kind,
            name,
            vm_id,
            uuid,
        } => {
            // Own the key-derived fields up front so `key` and
            // `chunk_values` can move into a `Malformed` record when a
            // chunk's value fails to decode.
            let name = name.to_string();
            let vm_id = vm_id.map(str::to_string);
            let uuid = uuid.to_string();
            match decode_cloud_init_value(&chunk_values) {
                Ok((meta, message)) => DiagnosticRecord::Decoded {
                    event: DiagnosticEvent {
                        agent: CLOUD_INIT_PREFIX.to_string(),
                        boot_epoch,
                        vm_id,
                        kind,
                        name,
                        event_id: uuid,
                        timestamp: meta
                            .get("ts")
                            .and_then(|t| t.as_str())
                            .map(str::to_string),
                        result: meta
                            .get("result")
                            .and_then(|r| r.as_str())
                            .map(str::to_string),
                        duration: meta.get("duration").and_then(|d| d.as_f64()),
                        message,
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

/// Marker preceding a cloud-init value's message field: `"msg":"`.
const CLOUD_INIT_MSG_MARKER: &str = "\"msg\":\"";

/// Decode a cloud-init event's chunk value(s) into `(metadata, message)`,
/// reading `ts`/`result`/`duration` from an untyped [`serde_json::Value`].
///
/// A single record is complete JSON, parsed directly. A multi-record event
/// was split mid-escape by cloud-init's `_break_down` (e.g. a `\n` cut into
/// `\` and `n`), so no chunk is valid JSON alone: recover each chunk's raw
/// escaped `msg` slice, concatenate, and unescape once; metadata comes from
/// the first chunk.
fn decode_cloud_init_value(
    chunks: &[String],
) -> Result<(serde_json::Value, String), String> {
    if let [only] = chunks {
        let value: serde_json::Value =
            serde_json::from_str(only).map_err(|e| e.to_string())?;
        let message = value
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        return Ok((value, message));
    }

    let mut escaped = String::new();
    for chunk in chunks {
        escaped.push_str(cloud_init_escaped_msg_slice(chunk)?);
    }
    let message: String = serde_json::from_str(&format!("\"{escaped}\""))
        .map_err(|e| e.to_string())?;

    Ok((cloud_init_chunk_metadata(&chunks[0])?, message))
}

/// Recover a chunk's raw (still-escaped) `msg` slice — the bytes between
/// the `"msg":"` marker and the closing `"}` — without unescaping.
fn cloud_init_escaped_msg_slice(chunk: &str) -> Result<&str, String> {
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

/// Parse a chunk's non-`msg` prefix (the portion before its `,"msg":"`
/// field, which is always valid JSON) into a [`serde_json::Value`].
fn cloud_init_chunk_metadata(chunk: &str) -> Result<serde_json::Value, String> {
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
    use crate::{KvpPool, PoolMode};
    use rstest::rstest;

    const AGENT: &str = "azure-init-0.1.0";
    const VM_ID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const EVENT_ID: &str = "8f3e9c4a-1b2c-4d5e-9f01-234567890abc";
    const BOOT_EPOCH: i64 = 1_700_000_000;
    const TIMESTAMP: &str = "2026-07-27T21:33:24.300Z";

    #[test]
    fn event_key_formats_and_classifies() {
        let formatted = format_event_key(
            AGENT,
            BOOT_EPOCH,
            VM_ID,
            RecordKind::Event,
            "user:create_user",
            EVENT_ID,
            TIMESTAMP,
        );
        assert_eq!(
            formatted,
            format!(
                "{AGENT}|{BOOT_EPOCH}|{VM_ID}|event|user:create_user|\
                 {EVENT_ID}|{TIMESTAMP}"
            )
        );
        assert!(matches!(
            classify_key(&formatted),
            KeyClass::Event {
                agent,
                boot_epoch,
                vm_id,
                kind,
                name,
                event_id,
                timestamp,
            } if agent == AGENT
                && boot_epoch == BOOT_EPOCH
                && vm_id == VM_ID
                && kind == RecordKind::Event
                && name == "user:create_user"
                && event_id == EVENT_ID
                && timestamp == TIMESTAMP
        ));
    }

    #[test]
    fn classify_round_trips_every_kind() {
        for expected in
            [RecordKind::Start, RecordKind::Finish, RecordKind::Event]
        {
            let key = format_event_key(
                AGENT,
                BOOT_EPOCH,
                VM_ID,
                expected.clone(),
                "span:event",
                EVENT_ID,
                TIMESTAMP,
            );
            assert!(matches!(
                classify_key(&key),
                KeyClass::Event { kind, .. } if kind == expected
            ));
        }
    }

    #[rstest]
    #[case::start(RecordKind::Start, "start")]
    #[case::finish(RecordKind::Finish, "finish")]
    #[case::event(RecordKind::Event, "event")]
    #[case::other(RecordKind::Other("compressed".to_string()), "compressed")]
    fn record_kind_renders_as_its_token(
        #[case] kind: RecordKind,
        #[case] token: &str,
    ) {
        assert_eq!(kind.to_string(), token);
        assert_eq!(
            serde_json::to_value(&kind).unwrap(),
            serde_json::json!(token)
        );
    }

    fn class_of(key: &str) -> &'static str {
        match classify_key(key) {
            KeyClass::Event { .. } => "event",
            KeyClass::CloudInit { .. } => "cloud-init",
            KeyClass::Malformed { .. } => "malformed",
            KeyClass::Raw => "raw",
        }
    }

    #[rstest]
    #[case::event("a|100|vm|event|name|id|ts", "event")]
    #[case::cloud_init(
        "CLOUD_INIT|1785187982|finish|name|vmid|uuid",
        "cloud-init"
    )]
    #[case::raw_single_segment("PROVISIONING_REPORT", "raw")]
    #[case::raw_too_few_segments("a|100|vm|event|name|id", "raw")]
    #[case::raw_too_many_segments("a|100|vm|event|name|id|ts|extra", "raw")]
    #[case::raw_non_numeric_boot_epoch("a|notnum|vm|event|name|id|ts", "raw")]
    #[case::malformed_bad_kind("a|100|vm|NOTAKIND|name|id|ts", "malformed")]
    #[case::malformed_other_kind("a|100|vm|nope|name|id|ts", "malformed")]
    #[case::cloud_init_custom_type(
        "CLOUD_INIT|100|compressed|name|vmid|uuid",
        "cloud-init"
    )]
    #[case::cloud_init_non_numeric_incarnation(
        "CLOUD_INIT|notnum|finish|name|vmid|uuid",
        "malformed"
    )]
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
            AGENT,
            BOOT_EPOCH,
            VM_ID,
            RecordKind::Start,
            "config:dump",
            EVENT_ID,
            TIMESTAMP,
        );

        let dumped = vec![
            (key.clone(), "part-one/".to_string()),
            (key.clone(), "part-two".to_string()),
            (
                "PROVISIONING_REPORT".to_string(),
                "result=success".to_string(),
            ),
            ("a|100|vm|NOPE|name|id|ts".to_string(), "junk".to_string()),
        ];

        let records = reassemble(dumped);
        assert_eq!(records.len(), 3);

        assert_eq!(
            records[0],
            DiagnosticRecord::Decoded {
                event: DiagnosticEvent {
                    agent: AGENT.to_string(),
                    boot_epoch: BOOT_EPOCH,
                    vm_id: Some(VM_ID.to_string()),
                    kind: RecordKind::Start,
                    name: "config:dump".to_string(),
                    event_id: EVENT_ID.to_string(),
                    timestamp: Some(TIMESTAMP.to_string()),
                    result: None,
                    duration: None,
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
                AGENT,
                BOOT_EPOCH,
                VM_ID,
                RecordKind::Start,
                "span:name",
                event_id,
                TIMESTAMP,
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
            DiagnosticRecord::Decoded { chunks: 1, .. }
        ));
        assert!(matches!(
            &records[1],
            DiagnosticRecord::Decoded { chunks: 1, .. }
        ));
    }

    #[rstest]
    #[case::indexed_chunk(
        "a|100|vm|event|name|id|ts|0",
        "a|100|vm|event|name|id|ts"
    )]
    #[case::indexed_chunk_multi_digit(
        "a|100|vm|event|name|id|ts|12",
        "a|100|vm|event|name|id|ts"
    )]
    #[case::single_event_unchanged(
        "a|100|vm|event|name|id|ts",
        "a|100|vm|event|name|id|ts"
    )]
    #[case::cloud_init_indexed_chunk(
        "CLOUD_INIT|1785187982|finish|mod|vmid|uuid|0",
        "CLOUD_INIT|1785187982|finish|mod|vmid|uuid"
    )]
    #[case::raw_unchanged("PROVISIONING_REPORT", "PROVISIONING_REPORT")]
    #[case::non_event_numeric_tail_unchanged("foo|3", "foo|3")]
    #[case::malformed_unchanged(
        "a|100|vm|NOPE|name|id|ts",
        "a|100|vm|NOPE|name|id|ts"
    )]
    #[case::malformed_indexed_chunk(
        "a|100|vm|NOPE|name|id|ts|0",
        "a|100|vm|NOPE|name|id|ts"
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
            AGENT,
            BOOT_EPOCH,
            VM_ID,
            RecordKind::Finish,
            "config:dump",
            EVENT_ID,
            TIMESTAMP,
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
            DiagnosticRecord::Decoded {
                event: DiagnosticEvent {
                    agent: AGENT.to_string(),
                    boot_epoch: BOOT_EPOCH,
                    vm_id: Some(VM_ID.to_string()),
                    kind: RecordKind::Finish,
                    name: "config:dump".to_string(),
                    event_id: EVENT_ID.to_string(),
                    timestamp: Some(TIMESTAMP.to_string()),
                    result: None,
                    duration: None,
                    message: "part-one/part-two/part-three".to_string(),
                },
                chunks: 3,
            }
        );
    }

    #[test]
    fn azure_event_value_is_full_message() {
        let key = format_event_key(
            AGENT,
            BOOT_EPOCH,
            VM_ID,
            RecordKind::Event,
            "user:create_user",
            EVENT_ID,
            TIMESTAMP,
        );
        assert!(matches!(
            classify_record(key, vec!["boom".to_string()]),
            DiagnosticRecord::Decoded { event, chunks: 1 }
            if event.kind == RecordKind::Event
                && event.message == "boom"
                && event.agent == AGENT
                && event.vm_id.as_deref() == Some(VM_ID)
                && event.timestamp.as_deref() == Some(TIMESTAMP)
        ));
    }

    #[test]
    fn azure_span_value_is_full_message() {
        let key = format_event_key(
            AGENT,
            BOOT_EPOCH,
            VM_ID,
            RecordKind::Finish,
            "config:write",
            EVENT_ID,
            TIMESTAMP,
        );
        assert!(matches!(
            classify_record(key, vec!["write_config completed".to_string()]),
            DiagnosticRecord::Decoded { event, chunks: 1 }
            if event.kind == RecordKind::Finish
                && event.message == "write_config completed"
        ));
    }

    #[test]
    fn azure_span_start_finish_pair_round_trips_through_writer() {
        let dir = tempfile::TempDir::new().unwrap();
        let store =
            KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe)
                .unwrap();
        let diag = DiagnosticsKvp::new(store, VM_ID, AGENT);

        // A span emits a start and a finish sharing one event_id.
        diag.write_event(
            RecordKind::Start,
            EVENT_ID,
            "provision:run",
            "starting provision",
        )
        .unwrap();
        diag.write_event(
            RecordKind::Finish,
            EVENT_ID,
            "provision:run",
            "provision completed",
        )
        .unwrap();

        let events = diag.events().unwrap();
        assert_eq!(events.len(), 2);

        assert_eq!(events[0].kind, RecordKind::Start);
        assert_eq!(events[0].message, "starting provision");
        assert_eq!(events[1].kind, RecordKind::Finish);
        assert_eq!(events[1].message, "provision completed");

        for event in &events {
            assert_eq!(event.event_id, EVENT_ID);
            assert_eq!(event.name, "provision:run");
            assert_eq!(event.agent, AGENT);
            assert_eq!(event.vm_id.as_deref(), Some(VM_ID));
        }
    }

    const CLOUD_INIT_VM_ID: &str = "0e5e179d-5341-478b-8456-fbb90621bdf8";
    const CLOUD_INIT_KEY_FINISH: &str = "CLOUD_INIT|1785187982|finish|modules-final/config-scripts_user|0e5e179d-5341-478b-8456-fbb90621bdf8|e5f01809-a7a3-4279-aa64-1f18e21eda6e";
    const CLOUD_INIT_VALUE_FINISH: &str = r#"{"name":"modules-final/config-scripts_user","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.0006448590000012189,"msg":"config-scripts_user ran successfully and took 0.001 seconds"}"#;

    #[test]
    fn cloud_init_key_classifies() {
        assert!(matches!(
            classify_key(CLOUD_INIT_KEY_FINISH),
            KeyClass::CloudInit { boot_epoch, kind, name, vm_id, uuid }
            if boot_epoch == 1785187982
                && kind == RecordKind::Finish
                && name == "modules-final/config-scripts_user"
                && vm_id == Some(CLOUD_INIT_VM_ID)
                && uuid == "e5f01809-a7a3-4279-aa64-1f18e21eda6e"
        ));
        assert!(matches!(
            classify_key(
                "CLOUD_INIT|1785187982|start|modules-config/foo|\
                 c4d4a08d-fe93-4c7a-9be6-9a38c212e212"
            ),
            KeyClass::CloudInit { boot_epoch, kind, name, vm_id, uuid }
            if boot_epoch == 1785187982
                && kind == RecordKind::Start
                && name == "modules-config/foo"
                && vm_id.is_none()
                && uuid == "c4d4a08d-fe93-4c7a-9be6-9a38c212e212"
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
        assert!(matches!(
            classify_record(
                CLOUD_INIT_KEY_FINISH.to_string(),
                vec![CLOUD_INIT_VALUE_FINISH.to_string()],
            ),
            DiagnosticRecord::Decoded { event, chunks: 1 }
            if event.agent == "CLOUD_INIT"
                && event.boot_epoch == 1785187982
                && event.kind == RecordKind::Finish
                && event.name == "modules-final/config-scripts_user"
                && event.vm_id.as_deref() == Some(CLOUD_INIT_VM_ID)
                && event.event_id == "e5f01809-a7a3-4279-aa64-1f18e21eda6e"
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
            DiagnosticRecord::Decoded {
                event: DiagnosticEvent {
                    agent: "CLOUD_INIT".to_string(),
                    boot_epoch: 1785187982,
                    vm_id: Some(CLOUD_INIT_VM_ID.to_string()),
                    kind: RecordKind::Start,
                    name: "modules-final/config-keys_to_console".to_string(),
                    event_id: "7792621b-b339-4274-8b71-2a3dcbd2db4e"
                        .to_string(),
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
    fn cloud_init_non_span_type_decodes_to_other_kind() {
        let key = format!(
            "CLOUD_INIT|1785187982|compressed|cloud-init.log|\
             {CLOUD_INIT_VM_ID}|abc12345-1111-2222-3333-444455556666"
        );
        let value = r#"{"name":"cloud-init.log","type":"compressed","ts":"2026-07-27T21:33:24.339006+00:00","msg":"payload"}"#;
        assert!(matches!(
            classify_record(key, vec![value.to_string()]),
            DiagnosticRecord::Decoded { event, chunks: 1 }
            if event.kind == RecordKind::Other("compressed".to_string())
                && event.agent == "CLOUD_INIT"
                && event.message == "payload"
        ));
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
            vec![DiagnosticRecord::Decoded {
                event: DiagnosticEvent {
                    agent: "CLOUD_INIT".to_string(),
                    boot_epoch: 1785187982,
                    vm_id: Some(CLOUD_INIT_VM_ID.to_string()),
                    kind: RecordKind::Finish,
                    name: "modules-final/long".to_string(),
                    event_id: "abc12345-1111-2222-3333-444455556666"
                        .to_string(),
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
            [DiagnosticRecord::Decoded { event, chunks: 2 }]
            if event.message == "line1\nline2"
                && event.result.as_deref() == Some("SUCCESS")
                && event.duration == Some(0.5)
        ));
    }
}
