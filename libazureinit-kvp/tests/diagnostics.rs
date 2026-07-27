// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the [`DiagnosticsKvp`] layer: emit/read
//! round-trips, chunk reassembly, classification, scoped clearing, and
//! the concurrent-write atomicity guarantee that keeps chunked events
//! from interleaving.

use std::thread;

use libazureinit_kvp::{
    DiagnosticEvent, DiagnosticRecord, DiagnosticsKvp, KvpPool, KvpPoolStore,
    PoolMode, MAX_CHUNK_BYTES,
};
use tempfile::TempDir;
use tracing::Level;

const PREFIX: &str = "azure-init-test";
const VM_ID: &str = "vm-abc";

fn diagnostics(dir: &TempDir) -> DiagnosticsKvp {
    let store =
        KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe)
            .unwrap();
    DiagnosticsKvp::new(store, VM_ID, PREFIX)
}

/// Real cloud-init reporting entries captured from a guest pool 1 file.
/// Each tuple is one record's `(key, JSON value)`.
const CLOUD_INIT_RECORDS: &[(&str, &str)] = &[
    (
        "CLOUD_INIT|1785187982|finish|modules-final/config-scripts_user|0e5e179d-5341-478b-8456-fbb90621bdf8|e5f01809-a7a3-4279-aa64-1f18e21eda6e",
        r#"{"name":"modules-final/config-scripts_user","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.0006448590000012189,"msg":"config-scripts_user ran successfully and took 0.001 seconds"}"#,
    ),
    (
        "CLOUD_INIT|1785187982|start|modules-final/config-ssh_authkey_fingerprints|0e5e179d-5341-478b-8456-fbb90621bdf8|c4d4a08d-fe93-4c7a-9be6-9a38c212e212",
        r#"{"name":"modules-final/config-ssh_authkey_fingerprints","type":"start","ts":"2026-07-27T21:33:24.339170+00:00","msg":"running config-ssh_authkey_fingerprints with frequency once-per-instance"}"#,
    ),
    (
        "CLOUD_INIT|1785187982|finish|modules-final|0e5e179d-5341-478b-8456-fbb90621bdf8|126f969f-13fd-4b4b-a136-b7114518491f",
        r#"{"name":"modules-final","type":"finish","ts":"2026-07-27T21:33:24.431885+00:00","result":"SUCCESS","duration":0.340712044,"msg":"running modules for final"}"#,
    ),
];

#[test]
fn reads_and_parses_real_cloud_init_pool() {
    let dir = TempDir::new().unwrap();
    let store =
        KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe)
            .unwrap();
    for &(key, value) in CLOUD_INIT_RECORDS {
        store.append(key, value).unwrap();
    }

    // A DiagnosticsKvp with no azure-init identity still reads cloud-init
    // entries written by another agent.
    let diagnostics = DiagnosticsKvp::new(store, "", "");
    let records = diagnostics.records().unwrap();
    assert_eq!(records.len(), CLOUD_INIT_RECORDS.len());

    // Every record decodes as a cloud-init event (none fall back to raw).
    for record in &records {
        assert!(matches!(record, DiagnosticRecord::CloudInit { .. }));
    }

    match &records[0] {
        DiagnosticRecord::CloudInit { event, chunks } => {
            assert_eq!(*chunks, 1);
            assert_eq!(event.event_type, "finish");
            assert_eq!(event.name, "modules-final/config-scripts_user");
            assert_eq!(
                event.vm_id.as_deref(),
                Some("0e5e179d-5341-478b-8456-fbb90621bdf8")
            );
            assert_eq!(event.result.as_deref(), Some("SUCCESS"));
            assert_eq!(
                event.message,
                "config-scripts_user ran successfully and took 0.001 seconds"
            );
        }
        other => panic!("expected cloud-init event, got {other:?}"),
    }

    // A `start` event carries neither result nor duration.
    match &records[1] {
        DiagnosticRecord::CloudInit { event, .. } => {
            assert_eq!(event.event_type, "start");
            assert!(event.result.is_none());
            assert!(event.duration.is_none());
        }
        other => panic!("expected cloud-init event, got {other:?}"),
    }
}

#[test]
fn short_event_round_trips_as_single_record() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    assert_eq!(diag.vm_id(), VM_ID);
    assert_eq!(diag.event_prefix(), PREFIX);

    let event =
        DiagnosticEvent::new(Level::INFO, "user:create_user", "created");
    diag.emit(&event).unwrap();

    assert_eq!(diag.store().dump().unwrap().len(), 1);

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 1);
    match &records[0] {
        DiagnosticRecord::Event {
            event: decoded,
            chunks,
        } => {
            assert_eq!(*chunks, 1);
            assert_eq!(decoded.level, Level::INFO);
            assert_eq!(decoded.name, "user:create_user");
            assert_eq!(decoded.event_id, event.event_id);
            assert_eq!(decoded.message, "created");
        }
        other => panic!("expected event, got {other:?}"),
    }
}

#[test]
fn long_event_splits_across_records_and_reassembles() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    let message = "x".repeat(MAX_CHUNK_BYTES * 3 + 50);
    let event = DiagnosticEvent::new(Level::DEBUG, "config:dump", &message);
    diag.emit(&event).unwrap();

    // Split across four records, each with a unique `|<subevent_index>`
    // key so the Hyper-V host (one record per key) keeps every chunk;
    // they share one event-key base.
    let dumped = diag.store().dump().unwrap();
    assert_eq!(dumped.len(), 4);
    let base_of = |k: &str| k.rsplit_once('|').unwrap().0.to_string();
    let base = base_of(&dumped[0].0);
    assert!(
        dumped.iter().all(|(k, _)| base_of(k) == base),
        "all chunks share one event-key base"
    );
    let mut keys: Vec<String> = dumped.iter().map(|(k, _)| k.clone()).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 4, "each chunk must have a unique key");

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 1);
    match &records[0] {
        DiagnosticRecord::Event {
            event: decoded,
            chunks,
        } => {
            assert_eq!(*chunks, 4);
            assert_eq!(decoded.message, message);
        }
        other => panic!("expected event, got {other:?}"),
    }
}

#[test]
fn multi_chunk_event_uses_unique_keys_so_host_keeps_all() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    let message = "z".repeat(MAX_CHUNK_BYTES * 2 + 1);
    diag.emit(&DiagnosticEvent::new(Level::INFO, "big:event", &message))
        .unwrap();

    // Three records, no two sharing a key: the Hyper-V host keeps only one
    // record per key, so shared keys would silently drop chunks.
    let dumped = diag.store().dump().unwrap();
    assert_eq!(dumped.len(), 3);
    let total = dumped.len();
    let mut keys: Vec<String> = dumped.into_iter().map(|(k, _)| k).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), total, "chunk keys must be unique");

    // The event still reassembles to the full message.
    let events = diag.events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, message);
}

#[test]
fn injected_malformed_key_is_classified() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    // Five segments but an unrecognized level.
    diag.store()
        .append(&format!("{PREFIX}|{VM_ID}|NOPE|bad:level|id"), "junk")
        .unwrap();

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0],
        DiagnosticRecord::Malformed { reason, .. } if reason.contains("NOPE")
    ));
}

#[test]
fn mixed_records_round_trip_together() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    diag.emit(&DiagnosticEvent::new(Level::INFO, "a:b", "short"))
        .unwrap();
    diag.emit(&DiagnosticEvent::new(
        Level::WARN,
        "c:d",
        "y".repeat(MAX_CHUNK_BYTES + 5),
    ))
    .unwrap();
    diag.store()
        .append("PROVISIONING_REPORT", "result=success")
        .unwrap();
    diag.store()
        .append(&format!("{PREFIX}|{VM_ID}|NOPE|e:f|id"), "junk")
        .unwrap();

    let records = diag.records().unwrap();
    // Two events + one raw + one malformed.
    assert_eq!(records.len(), 4);
    assert_eq!(diag.events().unwrap().len(), 2);
}

#[test]
fn clear_removes_events_but_keeps_raw() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    diag.emit(&DiagnosticEvent::new(Level::INFO, "a:b", "e1"))
        .unwrap();
    diag.emit(&DiagnosticEvent::new(
        Level::DEBUG,
        "c:d",
        "z".repeat(MAX_CHUNK_BYTES * 2),
    ))
    .unwrap();
    diag.store()
        .append("PROVISIONING_REPORT", "result=success")
        .unwrap();

    diag.clear().unwrap();

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0],
        DiagnosticRecord::Raw { key, .. } if key == "PROVISIONING_REPORT"
    ));
    assert!(diag.events().unwrap().is_empty());
}

#[test]
fn clear_removes_all_diagnostics_regardless_of_scope() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    diag.emit(&DiagnosticEvent::new(Level::INFO, "a:b", "mine"))
        .unwrap();
    // Events from a different agent/VM and a malformed event key are also
    // diagnostic keys, so clear() removes them too.
    diag.store()
        .append("other-agent|other-vm|INFO|x:y|id", "theirs")
        .unwrap();
    diag.store().append("p|vm|NOPE|c:d|id", "junk").unwrap();
    // A raw record survives.
    diag.store()
        .append("PROVISIONING_REPORT", "result=success")
        .unwrap();

    diag.clear().unwrap();

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0],
        DiagnosticRecord::Raw { key, .. } if key == "PROVISIONING_REPORT"
    ));
    assert!(diag.events().unwrap().is_empty());
}

#[test]
fn emit_rejects_delimiter_in_event_fields() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    // A pipe in the name would produce an ambiguous six-segment key.
    let event = DiagnosticEvent::new(Level::INFO, "a|b", "msg");
    assert!(diag.emit(&event).is_err());
    // Nothing was written.
    assert!(diag.store().dump().unwrap().is_empty());
}

#[test]
fn concurrent_multichunk_emits_reassemble_without_interleaving() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    const THREADS: usize = 5;
    const PER_THREAD: usize = 8;
    // Force three chunks per event.
    let len = MAX_CHUNK_BYTES * 2 + 7;

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let diag = diag.clone();
            let marker = (b'a' + t as u8) as char;
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    let message = marker.to_string().repeat(len);
                    let event = DiagnosticEvent::new(
                        Level::INFO,
                        format!("thread:{marker}"),
                        message,
                    );
                    diag.emit(&event).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let events = diag.events().unwrap();
    // If any event's chunks had been split by an interleaving writer, the
    // key would appear as multiple groups and the count would be wrong.
    assert_eq!(events.len(), THREADS * PER_THREAD);
    for event in &events {
        // Each message is homogeneous and full length: chunks stayed
        // contiguous on disk.
        assert_eq!(event.message.len(), len);
        let first = event.message.chars().next().unwrap();
        assert!(event.message.chars().all(|c| c == first));
        assert_eq!(event.name, format!("thread:{first}"));
    }
}
