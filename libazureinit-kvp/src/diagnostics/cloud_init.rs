// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! cloud-init reporting key format: a read-only decoder that normalizes
//! cloud-init telemetry into a [`DiagnosticEvent`]. Handles the current
//! `CLOUD_INIT|<incarnation>|<type>|<name>|<vm_id>|<uuid>` layout and the
//! older one without `vm_id`, and reassembles values cloud-init's
//! `_break_down` split mid-escape across records.

use super::{
    DiagnosticEvent, KeyFormat, RecordKind, CLOUD_INIT_PREFIX,
    EVENT_KEY_DELIMITER,
};

/// Marker preceding a cloud-init value's message field: `"msg":"`.
const CLOUD_INIT_MSG_MARKER: &str = "\"msg\":\"";

/// The cloud-init reporting key format (read-only).
pub(super) struct CloudInit;

/// The parsed fields of a cloud-init key.
struct Parsed<'a> {
    boot_epoch: i64,
    kind: RecordKind,
    name: &'a str,
    vm_id: Option<&'a str>,
    uuid: &'a str,
}

impl CloudInit {
    /// Parse a cloud-init key's fields, or explain why it is malformed.
    /// Assumes [`owns`](KeyFormat::owns) matched the shape; an unknown
    /// `type` becomes [`RecordKind::Other`].
    fn parse(key: &str) -> Result<Parsed<'_>, String> {
        let mut segments = key.split(EVENT_KEY_DELIMITER);
        let _prefix = segments.next();
        let (Some(incarnation), Some(event_type), Some(name), Some(fourth)) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err("cloud-init key has too few segments".to_string());
        };
        let (vm_id, uuid) = match (segments.next(), segments.next()) {
            (None, None) => (None, fourth),
            (Some(uuid), None) => (Some(fourth), uuid),
            _ => {
                return Err("cloud-init key has too many segments".to_string())
            }
        };
        let boot_epoch = incarnation.parse::<i64>().map_err(|_| {
            format!("non-numeric cloud-init incarnation {incarnation:?}")
        })?;
        let kind = event_type
            .parse::<RecordKind>()
            .unwrap_or_else(|()| RecordKind::Other(event_type.to_string()));
        Ok(Parsed {
            boot_epoch,
            kind,
            name,
            vm_id,
            uuid,
        })
    }
}

impl KeyFormat for CloudInit {
    fn owns(&self, base_key: &str) -> bool {
        base_key.split(EVENT_KEY_DELIMITER).next() == Some(CLOUD_INIT_PREFIX)
            && matches!(base_key.split(EVENT_KEY_DELIMITER).count(), 5 | 6)
    }

    fn decode(
        &self,
        base_key: &str,
        chunks: &[String],
    ) -> Result<DiagnosticEvent, String> {
        let parsed = Self::parse(base_key)?;
        let (meta, message) = decode_value(chunks)
            .map_err(|err| format!("invalid cloud-init JSON value: {err}"))?;
        Ok(DiagnosticEvent {
            agent: CLOUD_INIT_PREFIX.to_string(),
            boot_epoch: parsed.boot_epoch,
            vm_id: parsed.vm_id.map(str::to_string),
            kind: parsed.kind,
            name: parsed.name.to_string(),
            event_id: parsed.uuid.to_string(),
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
        })
    }
}

/// Decode a cloud-init event's chunk value(s) into `(metadata, message)`.
///
/// A single record is complete JSON. Across records, cloud-init's
/// `_break_down` splits mid-escape (e.g. `\n` cut into `\` + `n`), so no
/// chunk is valid JSON alone: concatenate the raw escaped `msg` slices,
/// unescape once, and take metadata from the first chunk.
fn decode_value(
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
        escaped.push_str(escaped_msg_slice(chunk)?);
    }
    let message: String = serde_json::from_str(&format!("\"{escaped}\""))
        .map_err(|e| e.to_string())?;

    Ok((chunk_metadata(&chunks[0])?, message))
}

/// Recover a chunk's raw (still-escaped) `msg` slice — the bytes between the
/// `"msg":"` marker and the closing `"}` — without unescaping.
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

/// Parse a chunk's non-`msg` prefix (the portion before its `,"msg":"`
/// field, which is always valid JSON) into a [`serde_json::Value`].
fn chunk_metadata(chunk: &str) -> Result<serde_json::Value, String> {
    let marker = format!(",{CLOUD_INIT_MSG_MARKER}");
    let end = chunk
        .find(&marker)
        .ok_or("chunk is missing a \"msg\" field")?;
    serde_json::from_str(&format!("{}}}", &chunk[..end]))
        .map_err(|e| e.to_string())
}
