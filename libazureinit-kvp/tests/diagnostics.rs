// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the [`DiagnosticsKvp`] layer: emit/read
//! round-trips, chunk reassembly, classification, scoped clearing, and
//! the concurrent-write atomicity guarantee that keeps chunked events
//! from interleaving.

use std::thread;

use libazureinit_kvp::{
    DiagnosticEvent, DiagnosticRecord, DiagnosticsKvp, KvpPool, KvpPoolStore,
    PoolMode, RecordKind, MAX_CHUNK_BYTES,
};
use rstest::rstest;
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

    let dumped = diag.store().dump().unwrap();
    assert_eq!(dumped.len(), 1);
    assert!(dumped[0].0.ends_with("|0"), "key: {}", dumped[0].0);

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

const COMPRESSED_LOG_CHUNKS: &[(&str, &str)] = &[
    (
        "CLOUD_INIT|1786047606|compressed|cloud-init.log|b7a822ba-4eea-46c0-b559-e84396101132|0",
        "{\"name\":\"cloud-init.log\",\"type\":\"compressed\",\"ts\":\"2026-08-06T20:20:13.479078Z\",\"msg_i\":0,\"msg\":\"{\\\"encoding\\\": \\\"gz+b64\\\", \\\"data\\\": \\\"eJxtV0uObDUMnbOKt4GHYjuJbYYIxAoYIQZx7EhIDBjAgN1ziumz1K2uqtY9lWuf3+XB++uwr2N/\\\\n4fEDfsb48vXLP3//8ef3f/37208///jrL7//8OVq2K09s4zekFEn71Lyp2fiBZsU1wxZ5zv+BpAa\\\\nwIzY128A1jhGRMikPJSsPJym6x5ih681gNwA6jzvnvK4U+6dhTdqu/Z4OsaY0y9xutDOkeIZDaw0\\\\nsKTytj1N9x3zVFjMoKnFtGaZNDCzganSNHnj8Hg4Am1eK4fcY2Nn7djuLuvJ5Iq5zVP2PPdWA78a\\\\n+CcyRXBfig14WKnskaNU6UWe1cDsBiZ0LamKNLIJIPyJVDl3nrHyzdh7lvt42F4DqQ3kfPXY2e9h\\\\nMdUyfhqYQDCJvsJoa4VY3KG1j5F3E7UGeC0GE9Xm5p0X5EzeRxYdnG+BCrV0F7Ht7qTeANrZ59Sk\\\\new4YjeU6mIjxSQNAnUZoGU1i2eDc3Ya71KQpo7m8UwTRfc/3oUFKk4c9cPbMoho6nhn+iQ8r1iQn\\\\nrdfAdrrAeeja0gxPp9w3d0FwV+LiVy4YVwMUH9hTp17qVOGQpwHZDs60VwTfqINRsY17Q2l9pmcR\\\\nj+4+qztrp5LYy/KCfmfbXbX4rlBfonb1NEZArRaW1+XDdtRTL0Nm+9LQLaI15t2Yx/tY18WJa3oD\\\\n22mDRtLaV1QGtHAfdvCSyavRKHVKyLev7u34WrOSISCLQQRnPgJ0sBskTw7Ts+hutlOBqN85Rplz\\\\nXuWwj6iyaAhjjIL7P5qqfM9rhEWdDl48ObTXmF44zUznfd+kby/nTgVF70HnA0sQ0h1SL/zZilP1\\\\nsAm4C6UW7\"}",
    ),
    (
        "CLOUD_INIT|1786047606|compressed|cloud-init.log|b7a822ba-4eea-46c0-b559-e84396101132|1",
        "{\"name\":\"cloud-init.log\",\"type\":\"compressed\",\"ts\":\"2026-08-06T20:20:13.479078Z\",\"msg_i\":1,\"msg\":\"ASEz7iBEzbQnUL8DD83hUtijQtvWjYOAogZfCGPdZMbtXKni0Ovzj2g/Ra4G3zZDVx5\\\\niKDbQHQqsHvXgQWB4ONOz21IHWKu3QB0fL8Yx8sNLlgG3Wv78IvN1xdpo0Xu+L6PrwejqVFX59R1\\\\nPgu86zGvSyDdXgQhra3I8Fmy2btddpwvQRbV2dc0RMaathBMrJCmyHIoynHqkQzv9VUb6x7Yazbw\\\\nnSIO3HFlbnWOElSMkncWwntin5KO1C5enuCOn6PdVDtN6COky/gkbjgh6su2R/qD/zFfHU0ccqcE\\\\nZKqmvPWK4Re+IovPxHi9tic+hU0DMO6zSfSpH9zcurRZYR+/NOS9ZwVxIRWLHzoFLB/rM4w469Nn\\\\nZoxN6ZMzpzdZIp1SRsbbG3Vj7YPocHpzudkUxxjxNQZCbIMxLKfAC1AuF0ljZdJpR8ZF3RvTEh0r\\\\nWN8q0Tu6y9tOlSOU5crBpfYxKA6y4ubyTjV8PsG639xy4Smw0oKp66B4oUdjolrFJ48uBFGjsVLp\\\\ndHQwaJ+JsISJokGIvIc6YBAkyLMPPIsnVo6CBKYf7QxVOhU5PDqPwe8hbFsuA2VSRx4nytmAtFqB\\\\n3SEDJbFIWM1aMI6gj82CNzJmV2ilU8dZajljvmFgSJrivtEeAaAo4rhlR/k5DLo1gK1KtvO8+TJj\\\\nfnxHH1wRwvj28tkp4RmthXGj7zAhqdCpBc8Ad+OMoIgQyTiINgGRdeNkzT5np4GHkNCZaFDCmDOC\\\\ntpYzTKFggG6JlhHPPiUykVQNaMf86yAGcgjJUQPlZI3/4xGPGPdsjsazZ6eAWHiUmugR8JI9HKx6\\\\n6NkPhfPDPopEZ1eFMTZbnZ0m8IQDlaPl/S8IG4GF8MJqcOD/AFeindw=\\\"}",
    ),
    (
        "CLOUD_INIT|1786047606|compressed|cloud-init.log|b7a822ba-4eea-46c0-b559-e84396101132|2",
        "{\"name\":\"cloud-init.log\",\"type\":\"compressed\",\"ts\":\"2026-08-06T20:20:13.479078Z\",\"msg_i\":2,\"msg\":\"\\n\\\"}\"}",
    ),
];

/// The reassembled `msg` across all three chunks.
const EXPECTED_COMPRESSED_MSG: &str = "{\"encoding\": \"gz+b64\", \"data\": \"eJxtV0uObDUMnbOKt4GHYjuJbYYIxAoYIQZx7EhIDBjAgN1ziumz1K2uqtY9lWuf3+XB++uwr2N/\\n4fEDfsb48vXLP3//8ef3f/37208///jrL7//8OVq2K09s4zekFEn71Lyp2fiBZsU1wxZ5zv+BpAa\\nwIzY128A1jhGRMikPJSsPJym6x5ih681gNwA6jzvnvK4U+6dhTdqu/Z4OsaY0y9xutDOkeIZDaw0\\nsKTytj1N9x3zVFjMoKnFtGaZNDCzganSNHnj8Hg4Am1eK4fcY2Nn7djuLuvJ5Iq5zVP2PPdWA78a\\n+CcyRXBfig14WKnskaNU6UWe1cDsBiZ0LamKNLIJIPyJVDl3nrHyzdh7lvt42F4DqQ3kfPXY2e9h\\nMdUyfhqYQDCJvsJoa4VY3KG1j5F3E7UGeC0GE9Xm5p0X5EzeRxYdnG+BCrV0F7Ht7qTeANrZ59Sk\\new4YjeU6mIjxSQNAnUZoGU1i2eDc3Ya71KQpo7m8UwTRfc/3oUFKk4c9cPbMoho6nhn+iQ8r1iQn\\nrdfAdrrAeeja0gxPp9w3d0FwV+LiVy4YVwMUH9hTp17qVOGQpwHZDs60VwTfqINRsY17Q2l9pmcR\\nj+4+qztrp5LYy/KCfmfbXbX4rlBfonb1NEZArRaW1+XDdtRTL0Nm+9LQLaI15t2Yx/tY18WJa3oD\\n22mDRtLaV1QGtHAfdvCSyavRKHVKyLev7u34WrOSISCLQQRnPgJ0sBskTw7Ts+hutlOBqN85Rplz\\nXuWwj6iyaAhjjIL7P5qqfM9rhEWdDl48ObTXmF44zUznfd+kby/nTgVF70HnA0sQ0h1SL/zZilP1\\nsAm4C6UW7ASEz7iBEzbQnUL8DD83hUtijQtvWjYOAogZfCGPdZMbtXKni0Ovzj2g/Ra4G3zZDVx5\\niKDbQHQqsHvXgQWB4ONOz21IHWKu3QB0fL8Yx8sNLlgG3Wv78IvN1xdpo0Xu+L6PrwejqVFX59R1\\nPgu86zGvSyDdXgQhra3I8Fmy2btddpwvQRbV2dc0RMaathBMrJCmyHIoynHqkQzv9VUb6x7Yazbw\\nnSIO3HFlbnWOElSMkncWwntin5KO1C5enuCOn6PdVDtN6COky/gkbjgh6su2R/qD/zFfHU0ccqcE\\nZKqmvPWK4Re+IovPxHi9tic+hU0DMO6zSfSpH9zcurRZYR+/NOS9ZwVxIRWLHzoFLB/rM4w469Nn\\nZoxN6ZMzpzdZIp1SRsbbG3Vj7YPocHpzudkUxxjxNQZCbIMxLKfAC1AuF0ljZdJpR8ZF3RvTEh0r\\nWN8q0Tu6y9tOlSOU5crBpfYxKA6y4ubyTjV8PsG639xy4Smw0oKp66B4oUdjolrFJ48uBFGjsVLp\\ndHQwaJ+JsISJokGIvIc6YBAkyLMPPIsnVo6CBKYf7QxVOhU5PDqPwe8hbFsuA2VSRx4nytmAtFqB\\n3SEDJbFIWM1aMI6gj82CNzJmV2ilU8dZajljvmFgSJrivtEeAaAo4rhlR/k5DLo1gK1KtvO8+TJj\\nfnxHH1wRwvj28tkp4RmthXGj7zAhqdCpBc8Ad+OMoIgQyTiINgGRdeNkzT5np4GHkNCZaFDCmDOC\\ntpYzTKFggG6JlhHPPiUykVQNaMf86yAGcgjJUQPlZI3/4xGPGPdsjsazZ6eAWHiUmugR8JI9HKx6\\n6NkPhfPDPopEZ1eFMTZbnZ0m8IQDlaPl/S8IG4GF8MJqcOD/AFeindw=\\n\"}";

const CLOUD_INIT_VM_ID: &str = "0e5e179d-5341-478b-8456-fbb90621bdf8";

/// Insert a `vm_id` segment after `name`, turning an old-format key into
/// the current layout (any trailing chunk index is preserved):
/// `CLOUD_INIT|inc|type|name|uuid[|i]`
///   -> `CLOUD_INIT|inc|type|name|vm_id|uuid[|i]`.
fn with_vm_id(old_key: &str, vm_id: &str) -> String {
    let mut segments: Vec<&str> = old_key.split('|').collect();
    segments.insert(4, vm_id); // 0:CLOUD_INIT 1:inc 2:type 3:name | vm_id
    segments.join("|")
}

fn without_vm_id(current_key: &str) -> String {
    let mut segments: Vec<&str> = current_key.split('|').collect();
    segments.remove(4);
    segments.join("|")
}

/// Append the given records to a fresh guest pool and classify them.
fn records_of<K: AsRef<str>, V: AsRef<str>>(
    pairs: &[(K, V)],
) -> Vec<DiagnosticRecord> {
    let dir = TempDir::new().unwrap();
    let store =
        KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe)
            .unwrap();
    for (key, value) in pairs {
        store.append(key.as_ref(), value.as_ref()).unwrap();
    }
    DiagnosticsKvp::new(store, "", "").records().unwrap()
}

/// Expect exactly one decoded event, returning it with its chunk count.
fn decode_single(records: Vec<DiagnosticRecord>) -> (DiagnosticEvent, usize) {
    assert_eq!(records.len(), 1, "expected one record, got: {records:?}");
    match records.into_iter().next().unwrap() {
        DiagnosticRecord::Decoded { event, chunks } => (event, chunks),
        other => panic!("expected a decoded event, got: {other:?}"),
    }
}

#[rstest]
#[case::start("start", "azure-ds", RecordKind::Start)]
#[case::finish("finish", "azure-ds/get-metadata", RecordKind::Finish)]
#[case::event("event", "user:create_user", RecordKind::Event)]
#[case::diagnostic(
    "diagnostic",
    "diagnostic message",
    RecordKind::Other("diagnostic".to_string())
)]
#[case::compressed(
    "compressed",
    "cloud-init.log",
    RecordKind::Other("compressed".to_string())
)]
#[case::boot_telemetry(
    "boot-telemetry",
    "boot-telemetry",
    RecordKind::Other("boot-telemetry".to_string())
)]
#[case::system_info(
    "system-info",
    "system information",
    RecordKind::Other("system-info".to_string())
)]
fn cloud_init_type_decodes_in_both_layouts(
    #[case] event_type: &str,
    #[case] name: &str,
    #[case] expected: RecordKind,
) {
    const TS: &str = "2026-08-06T20:20:13.479078Z";
    const UUID: &str = "b7a822ba-4eea-46c0-b559-e84396101132";
    let msg = format!("payload for {event_type}");
    let value = format!(
        "{{\"name\":\"{name}\",\"type\":\"{event_type}\",\
         \"ts\":\"{TS}\",\"msg\":\"{msg}\"}}"
    );
    let old_key = format!("CLOUD_INIT|1786047606|{event_type}|{name}|{UUID}");
    let current_key = with_vm_id(&old_key, CLOUD_INIT_VM_ID);

    let (event, chunks) = decode_single(records_of(&[(&old_key, &value)]));
    assert_eq!(chunks, 1);
    assert_eq!(event.agent, "CLOUD_INIT");
    assert_eq!(event.kind, expected);
    assert_eq!(event.vm_id, None);
    assert_eq!(event.name, name);
    assert_eq!(event.message, msg);

    let (event, _) = decode_single(records_of(&[(&current_key, &value)]));
    assert_eq!(event.kind, expected);
    assert_eq!(event.vm_id.as_deref(), Some(CLOUD_INIT_VM_ID));
    assert_eq!(event.message, msg);
}

#[test]
fn cloud_init_finish_reports_result_and_duration_in_both_layouts() {
    let value = "{\"name\":\"azure-ds/get-metadata\",\"type\":\"finish\",\
                 \"ts\":\"2026-08-06T20:20:13.400000Z\",\"result\":\"SUCCESS\",\
                 \"duration\":0.1234,\"msg\":\"finished\"}";
    let old_key = "CLOUD_INIT|1786047606|finish|azure-ds/get-metadata|\
                   b7a822ba-4eea-46c0-b559-e84396101132";

    for key in [old_key.to_string(), with_vm_id(old_key, CLOUD_INIT_VM_ID)] {
        let (event, _) = decode_single(records_of(&[(key.as_str(), value)]));
        assert_eq!(event.kind, RecordKind::Finish);
        assert_eq!(event.result.as_deref(), Some("SUCCESS"));
        assert_eq!(event.duration, Some(0.1234));
    }
}

#[test]
fn real_cloud_init_samples_decode_without_vm_id_too() {
    for &(key, value) in CLOUD_INIT_RECORDS {
        let (event, _) =
            decode_single(records_of(&[(without_vm_id(key), value)]));
        assert!(event.vm_id.is_none(), "stripped sample kept a vm_id: {key}");
    }
    let (event, _) = decode_single(records_of(&[CLOUD_INIT_RECORDS[0]]));
    assert_eq!(event.vm_id.as_deref(), Some(CLOUD_INIT_VM_ID));
}

#[test]
fn old_compressed_log_reassembles_across_chunks() {
    let (event, chunks) = decode_single(records_of(COMPRESSED_LOG_CHUNKS));
    assert_eq!(chunks, 3, "the three chunks must regroup into one event");
    assert_eq!(event.kind, RecordKind::Other("compressed".to_string()));
    assert_eq!(event.vm_id, None);
    assert_eq!(event.name, "cloud-init.log");
    assert_eq!(event.message, EXPECTED_COMPRESSED_MSG);
}

#[test]
fn current_compressed_log_reassembles_across_chunks() {
    let current: Vec<(String, &str)> = COMPRESSED_LOG_CHUNKS
        .iter()
        .map(|&(key, value)| (with_vm_id(key, CLOUD_INIT_VM_ID), value))
        .collect();
    let (event, chunks) = decode_single(records_of(&current));
    assert_eq!(chunks, 3);
    assert_eq!(event.kind, RecordKind::Other("compressed".to_string()));
    assert_eq!(event.vm_id.as_deref(), Some(CLOUD_INIT_VM_ID));
    assert_eq!(event.message, EXPECTED_COMPRESSED_MSG);
}

#[test]
fn cloud_init_event_with_invalid_json_is_still_flagged() {
    let key = format!(
        "CLOUD_INIT|1786047606|compressed|cloud-init.log|{CLOUD_INIT_VM_ID}|\
         b7a822ba-4eea-46c0-b559-e84396101132"
    );
    let records = records_of(&[(key.as_str(), "not-json")]);
    assert_eq!(records.len(), 1);
    assert!(
        matches!(
            &records[0],
            DiagnosticRecord::Malformed { reason, .. }
                if reason.contains("invalid cloud-init JSON")
        ),
        "expected a Malformed record, got: {:?}",
        records[0]
    );
}
