// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed diagnostics over the raw [`KvpPoolStore`](crate::KvpPoolStore).
//!
//! [`DiagnosticsKvp`] writes azure-init diagnostics and reads diagnostics from
//! both azure-init and cloud-init into one [`DiagnosticEvent`] schema. Pool
//! records that are unrelated to diagnostics or cannot be decoded are skipped;
//! callers that need a lossless view can use [`KvpPoolStore::dump`].
//!
//! Azure-init keys use this format:
//! `<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>`.
//! Every value chunk has a zero-based `|<chunk_index>` suffix.
//!
//! Cloud-init keys use
//! `CLOUD_INIT|<incarnation>|<type>|<name>|[<vm_id>|]<uuid>`, with the same
//! numeric suffix when chunked. A cloud-init chunk also stores its index in
//! the JSON `msg_i` field. The reader validates both indices before combining
//! the escaped `msg` fragments.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use uuid::Uuid;

use crate::{KvpError, KvpPoolStore};

const CLOUD_INIT_PREFIX: &str = "CLOUD_INIT";
const EVENT_KEY_DELIMITER: char = '|';
const CLOUD_INIT_MSG_MARKER: &str = "\"msg\":\"";

/// Maximum number of UTF-8 value bytes stored in one diagnostic record.
///
/// This conservative limit keeps records readable through the Hyper-V host
/// path. Longer messages are split at UTF-8 character boundaries.
pub const MAX_CHUNK_BYTES: usize = 1022;

/// The lifecycle or reporting kind of a diagnostic entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// A span opening.
    Start,
    /// A span closing.
    Finish,
    /// A point-in-time event.
    Event,
    /// Another source-specific reporting type, such as cloud-init's
    /// `compressed` or `system-info`.
    Other(String),
}

impl DiagnosticKind {
    fn from_token(token: &str) -> Self {
        match token {
            "start" => Self::Start,
            "finish" => Self::Finish,
            "event" => Self::Event,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Start => "start",
            Self::Finish => "finish",
            Self::Event => "event",
            Self::Other(token) => token,
        })
    }
}

impl serde::Serialize for DiagnosticKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// One normalized azure-init or cloud-init diagnostic entry.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub struct DiagnosticEvent {
    /// Reporting source, such as `azure-init-0.1.1` or `CLOUD_INIT`.
    pub agent: String,
    /// Unix epoch second at which this boot began.
    pub boot_epoch: i64,
    /// VM identifier. Older cloud-init keys do not contain one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<String>,
    /// Entry lifecycle or source-specific reporting kind.
    pub kind: DiagnosticKind,
    /// Logical event or span name.
    pub name: String,
    /// Identifier shared by all chunks and, for spans, related lifecycle
    /// entries.
    pub event_id: String,
    /// Time at which the entry occurred.
    pub timestamp: DateTime<Utc>,
    /// Source result, when supplied by cloud-init.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Source duration in seconds, when supplied by cloud-init.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Reassembled human-readable payload.
    pub message: String,
}

/// Typed diagnostic access over a KVP pool.
///
/// The boot epoch is resolved once at construction so emitting tracing events
/// does not read `/proc/stat` for every entry.
#[derive(Clone, Debug)]
pub struct DiagnosticsKvp {
    store: KvpPoolStore,
    vm_id: String,
    agent: String,
    boot_epoch: i64,
}

impl DiagnosticsKvp {
    /// Create an accessor for `store`, stamping new entries with `vm_id` and
    /// `agent`.
    pub fn new(
        store: KvpPoolStore,
        vm_id: impl Into<String>,
        agent: impl Into<String>,
    ) -> Result<Self, KvpError> {
        let boot_epoch = store.boot_epoch()?;
        Ok(Self {
            store,
            vm_id: vm_id.into(),
            agent: agent.into(),
            boot_epoch,
        })
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

    pub fn boot_epoch(&self) -> i64 {
        self.boot_epoch
    }

    /// Emit a point event using a fresh UUID and the current timestamp.
    pub fn emit_event(
        &self,
        name: impl AsRef<str>,
        message: impl AsRef<str>,
    ) -> Result<(), KvpError> {
        self.emit(
            DiagnosticKind::Event,
            name,
            Uuid::new_v4().to_string(),
            Utc::now(),
            message,
        )
    }

    /// Emit an azure-init diagnostic with caller-supplied lifecycle metadata.
    ///
    /// A tracing adapter can infer `kind` from its callback, retain one
    /// `event_id` for a span, and pass the timestamp captured when the callback
    /// occurred. Key formatting, chunking, and storage remain encapsulated
    /// here.
    pub fn emit(
        &self,
        kind: DiagnosticKind,
        name: impl AsRef<str>,
        event_id: impl AsRef<str>,
        timestamp: DateTime<Utc>,
        message: impl AsRef<str>,
    ) -> Result<(), KvpError> {
        let name = name.as_ref();
        let event_id = event_id.as_ref();
        let kind_token = kind.to_string();

        reject_delimiter("agent", &self.agent)?;
        reject_delimiter("vm_id", &self.vm_id)?;
        reject_delimiter("kind", &kind_token)?;
        reject_delimiter("name", name)?;
        reject_delimiter("event_id", event_id)?;

        let timestamp = timestamp.to_rfc3339_opts(SecondsFormat::Millis, true);
        let key = format_event_key(
            &self.agent,
            self.boot_epoch,
            &self.vm_id,
            &kind_token,
            name,
            event_id,
            &timestamp,
        );
        self.write_chunked(&key, message.as_ref())
    }

    /// Read all decodable diagnostics in first-seen pool order.
    ///
    /// Raw records, malformed entries, and incomplete chunk groups are omitted.
    pub fn entries(&self) -> Result<Vec<DiagnosticEvent>, KvpError> {
        Ok(decode_entries(self.store.dump()?))
    }

    fn write_chunked(&self, key: &str, value: &str) -> Result<(), KvpError> {
        let records = chunk_at_char_boundary(value, MAX_CHUNK_BYTES)
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| {
                (format!("{key}{EVENT_KEY_DELIMITER}{index}"), chunk)
            });
        self.store.append_multiple(records)
    }
}

fn reject_delimiter(field: &'static str, value: &str) -> Result<(), KvpError> {
    if value.contains(EVENT_KEY_DELIMITER) {
        return Err(KvpError::EventFieldContainsDelimiter { field });
    }
    Ok(())
}

fn format_event_key(
    agent: &str,
    boot_epoch: i64,
    vm_id: &str,
    kind: &str,
    name: &str,
    event_id: &str,
    timestamp: &str,
) -> String {
    let d = EVENT_KEY_DELIMITER;
    format!(
        "{agent}{d}{boot_epoch}{d}{vm_id}{d}{kind}{d}{name}{d}{event_id}{d}{timestamp}"
    )
}

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

struct AzureKey<'a> {
    agent: &'a str,
    boot_epoch: i64,
    vm_id: &'a str,
    kind: DiagnosticKind,
    name: &'a str,
    event_id: &'a str,
    timestamp: DateTime<Utc>,
}

struct CloudInitKey<'a> {
    boot_epoch: i64,
    kind: DiagnosticKind,
    name: &'a str,
    vm_id: Option<&'a str>,
    event_id: &'a str,
}

enum ParsedKey<'a> {
    Azure(AzureKey<'a>),
    CloudInit(CloudInitKey<'a>),
}

fn parse_diagnostic_key(key: &str) -> Option<ParsedKey<'_>> {
    if key.split(EVENT_KEY_DELIMITER).next()? == CLOUD_INIT_PREFIX {
        parse_cloud_init_key(key).map(ParsedKey::CloudInit)
    } else {
        parse_azure_key(key).map(ParsedKey::Azure)
    }
}

fn parse_azure_key(key: &str) -> Option<AzureKey<'_>> {
    let mut segments = key.split(EVENT_KEY_DELIMITER);
    let agent = segments.next()?;
    let boot_epoch = segments.next()?.parse().ok()?;
    let vm_id = segments.next()?;
    let kind = DiagnosticKind::from_token(segments.next()?);
    let name = segments.next()?;
    let event_id = segments.next()?;
    let timestamp = parse_timestamp(segments.next()?)?;
    if segments.next().is_some() {
        return None;
    }

    Some(AzureKey {
        agent,
        boot_epoch,
        vm_id,
        kind,
        name,
        event_id,
        timestamp,
    })
}

fn parse_cloud_init_key(key: &str) -> Option<CloudInitKey<'_>> {
    let segments: Vec<_> = key.split(EVENT_KEY_DELIMITER).collect();
    let (vm_id, event_id) = match segments.as_slice() {
        [CLOUD_INIT_PREFIX, _, _, _, event_id] => (None, *event_id),
        [CLOUD_INIT_PREFIX, _, _, _, vm_id, event_id] => {
            (Some(*vm_id), *event_id)
        }
        _ => return None,
    };

    Some(CloudInitKey {
        boot_epoch: segments[1].parse().ok()?,
        kind: DiagnosticKind::from_token(segments[2]),
        name: segments[3],
        vm_id,
        event_id,
    })
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// An indexed group is kept at the position where its first chunk appeared.
enum PendingEntry {
    Standalone {
        key: String,
        value: String,
    },
    Indexed {
        key: String,
        chunks: Vec<(u32, String)>,
    },
}

fn decode_entries(dumped: Vec<(String, String)>) -> Vec<DiagnosticEvent> {
    let mut pending = Vec::<PendingEntry>::new();
    let mut indexed_groups = HashMap::<String, usize>::new();

    for (key, value) in dumped {
        if let Some((base, index)) = split_chunk_index(&key) {
            let base = base.to_string();
            if let Some(position) = indexed_groups.get(&base).copied() {
                if let PendingEntry::Indexed { chunks, .. } =
                    &mut pending[position]
                {
                    chunks.push((index, value));
                }
            } else {
                indexed_groups.insert(base.clone(), pending.len());
                pending.push(PendingEntry::Indexed {
                    key: base,
                    chunks: vec![(index, value)],
                });
            }
        } else if parse_diagnostic_key(&key).is_some() {
            pending.push(PendingEntry::Standalone { key, value });
        }
    }

    pending
        .into_iter()
        .filter_map(|entry| match entry {
            PendingEntry::Standalone { key, value } => {
                decode_standalone(&key, &value)
            }
            PendingEntry::Indexed { key, chunks } => {
                decode_indexed(&key, chunks)
            }
        })
        .collect()
}

fn split_chunk_index(key: &str) -> Option<(&str, u32)> {
    let (base, index) = key.rsplit_once(EVENT_KEY_DELIMITER)?;
    let index = index.parse().ok()?;
    parse_diagnostic_key(base)?;
    Some((base, index))
}

fn order_chunks(mut chunks: Vec<(u32, String)>) -> Option<Vec<(u32, String)>> {
    chunks.sort_by_key(|(index, _)| *index);
    for (expected, (actual, _)) in chunks.iter().enumerate() {
        if *actual != u32::try_from(expected).ok()? {
            return None;
        }
    }
    Some(chunks)
}

fn decode_standalone(key: &str, value: &str) -> Option<DiagnosticEvent> {
    match parse_diagnostic_key(key)? {
        ParsedKey::Azure(key) => Some(azure_event(key, value.to_string())),
        ParsedKey::CloudInit(key) => decode_cloud_init_single(key, value),
    }
}

fn decode_indexed(
    key: &str,
    chunks: Vec<(u32, String)>,
) -> Option<DiagnosticEvent> {
    let chunks = order_chunks(chunks)?;
    match parse_diagnostic_key(key)? {
        ParsedKey::Azure(key) => Some(azure_event(
            key,
            chunks.into_iter().map(|(_, value)| value).collect(),
        )),
        ParsedKey::CloudInit(key) => decode_cloud_init_chunks(key, &chunks),
    }
}

fn azure_event(key: AzureKey<'_>, message: String) -> DiagnosticEvent {
    DiagnosticEvent {
        agent: key.agent.to_string(),
        boot_epoch: key.boot_epoch,
        vm_id: Some(key.vm_id.to_string()),
        kind: key.kind,
        name: key.name.to_string(),
        event_id: key.event_id.to_string(),
        timestamp: key.timestamp,
        result: None,
        duration: None,
        message,
    }
}

fn decode_cloud_init_single(
    key: CloudInitKey<'_>,
    value: &str,
) -> Option<DiagnosticEvent> {
    let metadata: serde_json::Value = serde_json::from_str(value).ok()?;
    if metadata.get("msg_i").is_some() {
        return None;
    }
    let message = metadata.get("msg")?.as_str()?.to_string();
    cloud_init_event(key, &metadata, message)
}

fn decode_cloud_init_chunks(
    key: CloudInitKey<'_>,
    chunks: &[(u32, String)],
) -> Option<DiagnosticEvent> {
    let mut metadata = None;
    let mut escaped_message = String::new();

    for (key_index, value) in chunks {
        let chunk_metadata = cloud_init_chunk_metadata(value)?;
        let value_index = chunk_metadata.get("msg_i")?.as_u64()?;
        if value_index != u64::from(*key_index) {
            return None;
        }
        if metadata.is_none() {
            metadata = Some(chunk_metadata);
        }
        escaped_message.push_str(cloud_init_escaped_msg_slice(value)?);
    }

    let message =
        serde_json::from_str(&format!("\"{escaped_message}\"")).ok()?;
    cloud_init_event(key, &metadata?, message)
}

fn cloud_init_event(
    key: CloudInitKey<'_>,
    metadata: &serde_json::Value,
    message: String,
) -> Option<DiagnosticEvent> {
    Some(DiagnosticEvent {
        agent: CLOUD_INIT_PREFIX.to_string(),
        boot_epoch: key.boot_epoch,
        vm_id: key.vm_id.map(str::to_string),
        kind: key.kind,
        name: key.name.to_string(),
        event_id: key.event_id.to_string(),
        timestamp: parse_timestamp(metadata.get("ts")?.as_str()?)?,
        result: metadata
            .get("result")
            .and_then(|result| result.as_str())
            .map(str::to_string),
        duration: metadata.get("duration").and_then(|value| value.as_f64()),
        message,
    })
}

/// Recover a cloud-init chunk's raw, still-escaped `msg` fragment.
fn cloud_init_escaped_msg_slice(chunk: &str) -> Option<&str> {
    let start =
        chunk.find(CLOUD_INIT_MSG_MARKER)? + CLOUD_INIT_MSG_MARKER.len();
    let end = chunk.strip_suffix("\"}")?.len();
    chunk.get(start..end)
}

/// Parse the valid metadata prefix before cloud-init's final `msg` field.
fn cloud_init_chunk_metadata(chunk: &str) -> Option<serde_json::Value> {
    let marker = format!(",{CLOUD_INIT_MSG_MARKER}");
    let end = chunk.find(&marker)?;
    serde_json::from_str(&format!("{}}}", &chunk[..end])).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const AGENT: &str = "azure-init-0.1.1";
    const VM_ID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const EVENT_ID: &str = "8f3e9c4a-1b2c-4d5e-9f01-234567890abc";
    const TIMESTAMP: &str = "2026-07-27T21:33:24.300Z";

    #[rstest]
    #[case(DiagnosticKind::Start, "start")]
    #[case(DiagnosticKind::Finish, "finish")]
    #[case(DiagnosticKind::Event, "event")]
    #[case(DiagnosticKind::Other("compressed".into()), "compressed")]
    fn kind_uses_wire_token(#[case] kind: DiagnosticKind, #[case] token: &str) {
        assert_eq!(kind.to_string(), token);
        assert_eq!(serde_json::to_value(kind).unwrap(), token);
    }

    #[rstest]
    #[case("", 4, vec![""])]
    #[case("abcdef", 2, vec!["ab", "cd", "ef"])]
    #[case("aéb", 2, vec!["a", "é", "b"])]
    #[case("€€", 1, vec!["€", "€"])]
    fn chunks_on_utf8_boundaries(
        #[case] input: &str,
        #[case] max: usize,
        #[case] expected: Vec<&str>,
    ) {
        assert_eq!(chunk_at_char_boundary(input, max), expected);
    }

    #[test]
    fn azure_key_round_trips() {
        let key = format_event_key(
            AGENT,
            1_700_000_000,
            VM_ID,
            "event",
            "user:create_user",
            EVENT_ID,
            TIMESTAMP,
        );
        let ParsedKey::Azure(parsed) = parse_diagnostic_key(&key).unwrap()
        else {
            panic!("expected azure-init key");
        };
        assert_eq!(parsed.agent, AGENT);
        assert_eq!(parsed.kind, DiagnosticKind::Event);
        assert_eq!(parsed.timestamp, parse_timestamp(TIMESTAMP).unwrap());
    }

    #[test]
    fn invalid_and_raw_records_are_skipped() {
        let valid = format_event_key(
            AGENT, 100, VM_ID, "event", "valid", EVENT_ID, TIMESTAMP,
        );
        let events = decode_entries(vec![
            ("PROVISIONING_REPORT".into(), "result=success".into()),
            ("a|not-a-boot|vm|event|name|id|timestamp".into(), "x".into()),
            (valid, "message".into()),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "message");
    }

    #[test]
    fn indexed_chunks_group_globally_and_preserve_first_seen_order() {
        let first = format_event_key(
            AGENT, 100, VM_ID, "event", "first", "id-1", TIMESTAMP,
        );
        let second = format_event_key(
            AGENT, 100, VM_ID, "event", "second", "id-2", TIMESTAMP,
        );
        let events = decode_entries(vec![
            (format!("{first}|1"), "b".into()),
            (format!("{second}|0"), "second".into()),
            (format!("{first}|0"), "a".into()),
        ]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "first");
        assert_eq!(events[0].message, "ab");
        assert_eq!(events[1].name, "second");
    }

    #[rstest]
    #[case(vec![(0, "a"), (2, "c")])]
    #[case(vec![(0, "a"), (0, "duplicate")])]
    #[case(vec![(1, "b")])]
    fn incomplete_or_duplicate_indices_are_skipped(
        #[case] chunks: Vec<(u32, &str)>,
    ) {
        let key = format_event_key(
            AGENT, 100, VM_ID, "event", "name", EVENT_ID, TIMESTAMP,
        );
        let dumped = chunks
            .into_iter()
            .map(|(index, value)| (format!("{key}|{index}"), value.into()))
            .collect();
        assert!(decode_entries(dumped).is_empty());
    }

    #[test]
    fn cloud_init_msg_i_must_match_key_index() {
        let base = "CLOUD_INIT|100|event|name|vm-id|event-id";
        let value = r#"{"name":"name","type":"event","ts":"2026-07-27T21:33:24Z","msg_i":1,"msg":"value"}"#;
        assert!(decode_entries(vec![(format!("{base}|0"), value.into())])
            .is_empty());
    }

    #[test]
    fn cloud_init_split_escape_is_unescaped_after_reassembly() {
        let base = "CLOUD_INIT|100|finish|name|vm-id|event-id";
        let events = decode_entries(vec![
            (
                format!("{base}|0"),
                r#"{"name":"name","type":"finish","ts":"2026-07-27T21:33:24Z","msg_i":0,"msg":"line1\"}"#
                    .into(),
            ),
            (
                format!("{base}|1"),
                r#"{"name":"name","type":"finish","ts":"2026-07-27T21:33:24Z","msg_i":1,"msg":"nline2"}"#
                    .into(),
            ),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "line1\nline2");
    }
}
