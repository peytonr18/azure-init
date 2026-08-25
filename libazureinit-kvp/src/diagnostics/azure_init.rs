// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! azure-init native key format: the canonical key the crate both writes
//! ([`format_event_key`]) and reads ([`AzureInit`]). It is the only format
//! this crate writes.

use super::{
    DiagnosticEvent, KeyFormat, RecordKind, CLOUD_INIT_PREFIX,
    EVENT_KEY_DELIMITER,
};

/// Number of `|`-delimited segments in an azure-init event key.
const SEGMENT_COUNT: usize = 7;

/// Format the canonical azure-init key
/// `<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>`;
/// [`AzureInit`] is the inverse. This logical pipe string is what
/// [`KvpPoolStore`](crate::KvpPoolStore) encodes for both disk and wire.
pub(super) fn format_event_key(
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

/// The azure-init native key format: the read side of [`format_event_key`].
pub(super) struct AzureInit;

/// The seven segments of an azure-init key; `kind` is validated in
/// [`AzureInit::decode`], not here.
struct Parsed<'a> {
    agent: &'a str,
    boot_epoch: i64,
    vm_id: &'a str,
    kind: &'a str,
    name: &'a str,
    event_id: &'a str,
    timestamp: &'a str,
}

impl AzureInit {
    /// Parse an azure-init-shaped key (shape only), or `None` for a
    /// `CLOUD_INIT` prefix, wrong segment count, or non-numeric boot epoch.
    /// A bad `kind` still parses here; [`decode`](KeyFormat::decode) rejects
    /// it.
    fn parse(key: &str) -> Option<Parsed<'_>> {
        if key.split(EVENT_KEY_DELIMITER).count() != SEGMENT_COUNT {
            return None;
        }
        let mut segments = key.split(EVENT_KEY_DELIMITER);
        let agent = segments.next()?;
        if agent == CLOUD_INIT_PREFIX {
            return None;
        }
        let boot_epoch = segments.next()?.parse::<i64>().ok()?;
        Some(Parsed {
            agent,
            boot_epoch,
            vm_id: segments.next()?,
            kind: segments.next()?,
            name: segments.next()?,
            event_id: segments.next()?,
            timestamp: segments.next()?,
        })
    }
}

impl KeyFormat for AzureInit {
    fn owns(&self, base_key: &str) -> bool {
        Self::parse(base_key).is_some()
    }

    fn decode(
        &self,
        base_key: &str,
        chunks: &[String],
    ) -> Result<DiagnosticEvent, String> {
        let parsed =
            Self::parse(base_key).ok_or("key is not azure-init-shaped")?;
        let kind = parsed
            .kind
            .parse::<RecordKind>()
            .map_err(|()| format!("unrecognized kind {:?}", parsed.kind))?;
        Ok(DiagnosticEvent {
            agent: parsed.agent.to_string(),
            boot_epoch: parsed.boot_epoch,
            vm_id: Some(parsed.vm_id.to_string()),
            kind,
            name: parsed.name.to_string(),
            event_id: parsed.event_id.to_string(),
            timestamp: Some(parsed.timestamp.to_string()),
            result: None,
            duration: None,
            message: chunks.concat(),
        })
    }
}
