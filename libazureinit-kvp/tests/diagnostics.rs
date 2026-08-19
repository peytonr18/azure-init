// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the [`DiagnosticsKvp`] layer: emit/read
//! round-trips, chunk reassembly, classification, scoped clearing, and
//! the concurrent-write atomicity guarantee that keeps chunked events
//! from interleaving.

use std::thread;

use libazureinit_kvp::{
    DiagnosticRecord, DiagnosticsKvp, KvpPool, KvpPoolStore, PoolMode,
    RecordKind, MAX_CHUNK_BYTES,
};
use tempfile::TempDir;

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

    let diagnostics = DiagnosticsKvp::new(store, "", "");
    let records = diagnostics.records().unwrap();
    assert_eq!(records.len(), CLOUD_INIT_RECORDS.len());

    for record in &records {
        assert!(matches!(record, DiagnosticRecord::Decoded { .. }));
    }

    match &records[0] {
        DiagnosticRecord::Decoded { event, chunks } => {
            assert_eq!(*chunks, 1);
            assert_eq!(event.agent, "CLOUD_INIT");
            assert_eq!(event.kind, RecordKind::Finish);
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
        other => panic!("expected event, got {other:?}"),
    }

    match &records[1] {
        DiagnosticRecord::Decoded { event, .. } => {
            assert_eq!(event.kind, RecordKind::Start);
            assert!(event.result.is_none());
            assert!(event.duration.is_none());
        }
        other => panic!("expected event, got {other:?}"),
    }
}

#[test]
fn short_event_round_trips_as_single_record() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    assert_eq!(diag.vm_id(), VM_ID);
    assert_eq!(diag.agent(), PREFIX);

    diag.emit_event("user:create_user", "created").unwrap();

    assert_eq!(diag.store().dump().unwrap().len(), 1);

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 1);
    match &records[0] {
        DiagnosticRecord::Decoded {
            event: decoded,
            chunks,
        } => {
            assert_eq!(*chunks, 1);
            assert_eq!(decoded.kind, RecordKind::Event);
            assert_eq!(decoded.vm_id.as_deref(), Some(VM_ID));
            assert_eq!(decoded.boot_epoch, diag.store().boot_epoch().unwrap());
            assert_eq!(decoded.name, "user:create_user");
            let event_id = uuid::Uuid::parse_str(&decoded.event_id)
                .expect("event_id should be a valid UUID");
            assert_eq!(
                event_id.get_version_num(),
                4,
                "event_id should be a UUIDv4"
            );
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
    diag.emit_event("config:dump", &message).unwrap();

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
        DiagnosticRecord::Decoded {
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
    diag.emit_event("big:event", &message).unwrap();

    let dumped = diag.store().dump().unwrap();
    assert_eq!(dumped.len(), 3);
    let total = dumped.len();
    let mut keys: Vec<String> = dumped.into_iter().map(|(k, _)| k).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), total, "chunk keys must be unique");

    let events = diag.events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, message);
}

#[test]
fn injected_malformed_key_is_classified() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    diag.store()
        .append(&format!("{PREFIX}|100|{VM_ID}|NOPE|bad:kind|id|ts"), "junk")
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

    diag.emit_event("a:b", "short").unwrap();
    diag.emit_event("c:d", "y".repeat(MAX_CHUNK_BYTES + 5))
        .unwrap();
    diag.store()
        .append("PROVISIONING_REPORT", "result=success")
        .unwrap();
    diag.store()
        .append(&format!("{PREFIX}|100|{VM_ID}|NOPE|e:f|id|ts"), "junk")
        .unwrap();

    let records = diag.records().unwrap();
    assert_eq!(records.len(), 4);
    assert_eq!(diag.events().unwrap().len(), 2);
}

#[test]
fn clear_removes_events_but_keeps_raw() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    diag.emit_event("a:b", "e1").unwrap();
    diag.emit_event("c:d", "z".repeat(MAX_CHUNK_BYTES * 2))
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

    diag.emit_event("a:b", "mine").unwrap();
    diag.store()
        .append("other-agent|100|other-vm|event|x:y|id|ts", "theirs")
        .unwrap();
    diag.store()
        .append("p|100|vm|NOPE|c:d|id|ts", "junk")
        .unwrap();
    diag.store()
        .append("p|100|vm|NOPE|c:d|id|ts|0", "junk-0")
        .unwrap();
    diag.store()
        .append("p|100|vm|NOPE|c:d|id|ts|1", "junk-1")
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
fn emit_rejects_delimiter_in_event_fields() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    assert!(diag.emit_event("a|b", "msg").is_err());
    assert!(diag.store().dump().unwrap().is_empty());
}

#[test]
fn concurrent_multichunk_emits_reassemble_without_interleaving() {
    let dir = TempDir::new().unwrap();
    let diag = diagnostics(&dir);

    const THREADS: usize = 5;
    const PER_THREAD: usize = 8;
    let len = MAX_CHUNK_BYTES * 2 + 7;

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let diag = diag.clone();
            let marker = (b'a' + t as u8) as char;
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    let message = marker.to_string().repeat(len);
                    diag.emit_event(format!("thread:{marker}"), message)
                        .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let events = diag.events().unwrap();
    assert_eq!(events.len(), THREADS * PER_THREAD);
    for event in &events {
        assert_eq!(event.message.len(), len);
        let first = event.message.chars().next().unwrap();
        assert!(event.message.chars().all(|c| c == first));
        assert_eq!(event.name, format!("thread:{first}"));
    }
}
