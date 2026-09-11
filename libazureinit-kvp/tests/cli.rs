// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs;
use std::io::Write;
use std::process::{Command, Output};

use libazureinit_kvp::{
    DiagnosticWriter, Encoding, KvpPool, KvpPoolStore, Outcome, PoolMode,
    PROVISIONING_REPORT_KEY,
};
use rstest::rstest;
use serde_json::{json, Value};
use tempfile::TempDir;

const VM_ID: &str = "0e5e179d-5341-478b-8456-fbb90621bdf8";

fn kvp(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_libazureinit-kvp"))
        .args(args)
        .output()
        .unwrap()
}

fn kvp_with_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_libazureinit-kvp"))
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn with_dir<'a>(dir: &'a TempDir, args: &'a [&'a str]) -> Vec<&'a str> {
    let mut all = vec!["--dir", dir.path().to_str().unwrap()];
    all.extend_from_slice(args);
    all
}

fn assert_success(output: Output) -> String {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_json(output: Output) -> Value {
    serde_json::from_str(&assert_success(output)).unwrap()
}

fn store_at(dir: &TempDir) -> KvpPoolStore {
    KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe).unwrap()
}

#[test]
fn help_lists_commands() {
    let stdout = assert_success(kvp(&["--help"]));
    assert!(stdout.contains("Usage: libazureinit-kvp"));
    assert!(stdout.contains("write"));
    assert!(stdout.contains("append-multiple"));
    assert!(stdout.contains("delete-multiple"));
    assert!(stdout.contains("is-stale"));
}

#[test]
fn write_help_documents_append_flag() {
    let stdout = assert_success(kvp(&["write", "--help"]));
    assert!(stdout.contains("--append"));
    assert!(stdout.contains("<KEY>"));
    assert!(stdout.contains("<VALUE>"));
}

#[test]
fn default_info_uses_default_store_constructor() {
    let stdout = assert_success(kvp(&["info"]));
    assert!(stdout.contains("pool=guest"));
    assert!(stdout.contains("path=/var/lib/hyperv/.kvp_pool_1"));
    assert!(stdout.contains("mode=safe"));
}

#[test]
fn info_reports_custom_store_metadata() {
    let dir = TempDir::new().unwrap();
    let args = with_dir(&dir, &["--pool", "external", "--unsafe", "info"]);
    let stdout = assert_success(kvp(&args));

    assert!(stdout.contains("pool=external"));
    assert!(stdout.contains(&format!(
        "path={}",
        dir.path().join(".kvp_pool_0").display()
    )));
    assert!(stdout.contains("mode=unsafe"));
    assert!(stdout.contains("records=0"));
    assert!(stdout.contains("empty=true"));
    assert!(stdout.contains("stale=false"));
    assert!(stdout.contains("max_key_size=512"));
    assert!(stdout.contains("max_value_size=2048"));
}

#[test]
fn info_accepts_equals_style_global_options() {
    let dir = TempDir::new().unwrap();
    let dir_arg = format!("--dir={}", dir.path().display());
    let stdout = assert_success(kvp(&[&dir_arg, "--pool=auto", "info"]));

    assert!(stdout.contains("pool=auto"));
    assert!(stdout.contains(&format!(
        "path={}",
        dir.path().join(".kvp_pool_2").display()
    )));
}

#[test]
fn write_append_read_dump_entries_delete_and_clear() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp(&with_dir(&dir, &["write", "a", "1"])));
    assert_success(kvp(&with_dir(&dir, &["write", "--append", "a", "2"])));

    assert_eq!(assert_success(kvp(&with_dir(&dir, &["read", "a"]))), "2\n");
    assert_eq!(
        assert_json(kvp(&with_dir(&dir, &["dump"]))),
        json!([{"key": "a", "value": "1"}, {"key": "a", "value": "2"}])
    );
    assert_eq!(assert_success(kvp(&with_dir(&dir, &["entries"]))), "a=2\n");
    assert_eq!(
        assert_success(kvp(&with_dir(&dir, &["delete", "a"]))),
        "true\n"
    );
    assert_eq!(assert_json(kvp(&with_dir(&dir, &["dump"]))), json!([]));

    assert_success(kvp(&with_dir(&dir, &["write", "b", "3"])));
    assert_success(kvp(&with_dir(&dir, &["clear"])));
    assert_eq!(assert_json(kvp(&with_dir(&dir, &["dump"]))), json!([]));
}

#[test]
fn load_replaces_pool_from_file() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("records.txt");
    fs::write(&input, "a=1\nb=2\n").unwrap();

    assert_success(kvp(&with_dir(
        &dir,
        &["load", "--file", input.to_str().unwrap()],
    )));
    assert_eq!(
        assert_success(kvp(&with_dir(&dir, &["dump", "--text"]))),
        "a=1\nb=2\n"
    );
}

#[test]
fn load_can_read_from_stdin() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp_with_stdin(&with_dir(&dir, &["load"]), "x=1\ny=2\n"));

    assert_eq!(
        assert_success(kvp(&with_dir(&dir, &["entries"]))),
        "x=1\ny=2\n"
    );
}

#[test]
fn append_multiple_can_read_from_stdin() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp(&with_dir(&dir, &["write", "x", "1"])));
    assert_success(kvp_with_stdin(
        &with_dir(&dir, &["append-multiple"]),
        "x=2\ny=3\n",
    ));

    assert_eq!(
        assert_success(kvp(&with_dir(&dir, &["dump", "--text"]))),
        "x=1\nx=2\ny=3\n"
    );
}

#[test]
fn append_multiple_can_read_from_file() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("records.txt");
    fs::write(&input, "a=1\nb=2\n").unwrap();

    assert_success(kvp(&with_dir(
        &dir,
        &["append-multiple", "--file", input.to_str().unwrap()],
    )));
    assert_eq!(
        assert_success(kvp(&with_dir(&dir, &["dump", "--text"]))),
        "a=1\nb=2\n"
    );
}

#[test]
fn delete_multiple_prints_removed_record_count() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp_with_stdin(
        &with_dir(&dir, &["append-multiple"]),
        "a=1\nb=2\na=3\n",
    ));

    assert_eq!(
        assert_success(kvp(&with_dir(&dir, &["delete-multiple", "a", "z"]))),
        "2\n"
    );
    assert_eq!(assert_success(kvp(&with_dir(&dir, &["entries"]))), "b=2\n");
}

#[test]
fn clear_if_stale_and_is_stale_use_status_apis() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp(&with_dir(&dir, &["clear", "--if-stale"])));

    let output = kvp(&with_dir(&dir, &["is-stale"]));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "false\n");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn read_missing_exits_one_without_output() {
    let dir = TempDir::new().unwrap();
    let output = kvp(&with_dir(&dir, &["read", "missing"]));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn validation_errors_exit_two() {
    let dir = TempDir::new().unwrap();
    let output = kvp(&with_dir(&dir, &["write", "", "value"]));
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("KVP key must not be empty"));
}

#[test]
fn json_read_round_trips_value_with_equals_and_newline() {
    let dir = TempDir::new().unwrap();
    let raw_value = "https://example.test/q=1\nline2";
    let status =
        std::process::Command::new(env!("CARGO_BIN_EXE_libazureinit-kvp"))
            .args(with_dir(&dir, &["write", "url", raw_value]))
            .status()
            .unwrap();
    assert!(status.success());

    let stdout =
        assert_success(kvp(&with_dir(&dir, &["--json", "read", "url"])));
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("read --json parses");
    assert_eq!(value["key"], "url");
    assert_eq!(value["value"], raw_value);
}

#[test]
fn report_success_defaults_agent_and_accepts_supporting_data() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp(&with_dir(
        &dir,
        &[
            "report-success",
            "--vm-id",
            "vm-1",
            "--supporting-data",
            "build=123,commit=abc",
        ],
    )));

    let report =
        assert_success(kvp(&with_dir(&dir, &["read", "PROVISIONING_REPORT"])));
    let expected_agent =
        format!("agent=libazureinit-kvp/{}", env!("CARGO_PKG_VERSION"));
    assert!(report.contains(&expected_agent), "report was: {report}");
    assert!(report.contains("vm_id=vm-1"), "report was: {report}");
    assert!(report.trim_end().ends_with("|build=123|commit=abc"));
}

#[test]
fn report_failure_rejects_invalid_supporting_data() {
    let dir = TempDir::new().unwrap();
    let output = kvp(&with_dir(
        &dir,
        &[
            "report-failure",
            "--vm-id",
            "vm-1",
            "--reason",
            "boom",
            "--supporting-data",
            "novalue",
        ],
    ));
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("key=value"));
}

#[test]
fn parsed_dump_reassembles_and_filters_without_dropping_other_entries() {
    let dir = TempDir::new().unwrap();
    let base = format!(
        "DIAG_V1|agent|{VM_ID}|event|a:b|e5f01809-a7a3-4279-aa64-1f18e21eda6e|2026-08-31T00:00:00.000Z|none||"
    );
    for (key, value) in [
        (format!("{base}|1"), "two"),
        ("note".into(), "raw value"),
        (format!("{base}|0"), "one/"),
        ("DIAG_V2|future".into(), "preserved"),
    ] {
        assert_success(kvp(&with_dir(
            &dir,
            &["write", "--append", &key, value],
        )));
    }
    assert_success(kvp(&with_dir(&dir, &["report-success", "--vm-id", VM_ID])));
    assert_success(kvp(&with_dir(
        &dir,
        &[
            "emit",
            "--name",
            "ssh:key",
            "--message",
            "added",
            "--vm-id",
            VM_ID,
        ],
    )));
    let path = store_at(&dir).path().to_path_buf();
    let before = fs::read(&path).unwrap();

    let entries = assert_json(kvp(&with_dir(&dir, &["dump", "--parse"])));
    assert_eq!(entries.as_array().unwrap().len(), 5);
    assert_eq!(entries[0]["type"], "diagnostic");
    assert_eq!(entries[0]["kind"], "event");
    assert_eq!(entries[0]["name"], "a:b");
    assert_eq!(entries[0]["payload"], "one/two");
    let timed_entries = &entries.as_array().unwrap()[1..3];
    assert!(timed_entries.iter().any(|entry| {
        entry["type"] == "PROVISIONING_REPORT" && entry["result"] == "success"
    }));
    assert!(timed_entries.iter().any(|entry| entry["name"] == "ssh:key"));
    assert_eq!(
        entries[3],
        json!({"type": "raw", "key": "note", "value": "raw value"})
    );
    assert_eq!(
        entries[4],
        json!({
            "type": "raw", "key": "DIAG_V2|future", "value": "preserved",
            "error": "unsupported_version",
        })
    );

    let filtered = assert_json(kvp(&with_dir(
        &dir,
        &["dump", "--parse", "--name", "ssh"],
    )));
    assert_eq!(
        filtered,
        Value::Array(entries.as_array().unwrap()[1..].to_vec())
    );
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn parsed_dump_sorts_timestamps_without_reordering_reader_or_raw_dump() {
    let dir = TempDir::new().unwrap();
    let store = store_at(&dir);
    let event_id = "e5f01809-a7a3-4279-aa64-1f18e21eda6e";
    let key = |name: &str, timestamp: &str| {
        format!("DIAG_V1|agent|{VM_ID}|event|{name}|{event_id}|{timestamp}|none|||0")
    };
    let records = vec![
        ("note".into(), "raw first"),
        (key("latest", "2026-08-31T00:00:03.000Z"), "latest"),
        (
            format!("CLOUD_INIT|100|event|cloud|{VM_ID}|{event_id}"),
            r#"{"name":"cloud","type":"event","ts":"2026-08-31T00:00:02.000500Z","msg":"cloud"}"#,
        ),
        (
            PROVISIONING_REPORT_KEY.into(),
            "result=success|agent=agent|vm_id=vm|pps_type=None|timestamp=2026-08-31T02:00:02+02:00",
        ),
        ("DIAG_V2|future".into(), "raw second"),
        (key("tie-first", "2026-08-31T00:00:02.000Z"), "first tie"),
        (key("earliest", "2026-08-31T00:00:01.000Z"), "earliest"),
        (key("tie-second", "2026-08-31T00:00:02.000Z"), "second tie"),
    ];
    store
        .append_multiple(records.iter().map(|(key, value)| (key, *value)))
        .unwrap();
    let before = fs::read(store.path()).unwrap();
    let reader_entries = serde_json::to_value(
        libazureinit_kvp::DiagnosticReader::new(store.clone())
            .entries()
            .unwrap(),
    )
    .unwrap();
    let expected = Value::Array(
        [6, 3, 5, 7, 2, 1, 0, 4]
            .into_iter()
            .map(|position| reader_entries[position].clone())
            .collect(),
    );
    let parsed = assert_json(kvp(&with_dir(&dir, &["dump", "--parse"])));
    assert_eq!(parsed, expected);
    assert_eq!(parsed[1]["timestamp"], "2026-08-31T02:00:02+02:00");

    let text =
        assert_success(kvp(&with_dir(&dir, &["dump", "--parse", "--text"])));
    let expected_labels = [
        "name=earliest ",
        "PROVISIONING_REPORT=",
        "name=tie-first ",
        "name=tie-second ",
        "name=cloud ",
        "name=latest ",
        "raw key=note ",
        "raw key=DIAG_V2|future ",
    ];
    assert_eq!(text.lines().count(), expected_labels.len());
    for (line, label) in text.lines().zip(expected_labels) {
        assert!(line.contains(label), "expected {label:?} in {line:?}");
    }

    let physical = assert_json(kvp(&with_dir(&dir, &["dump"])));
    let expected_physical: Vec<_> = records
        .iter()
        .map(|(key, value)| json!({"key": key, "value": value}))
        .collect();
    assert_eq!(physical, Value::Array(expected_physical));
    assert_eq!(fs::read(store.path()).unwrap(), before);
}

#[test]
fn parsed_dump_normalizes_cloud_init_in_json_and_text() {
    let dir = TempDir::new().unwrap();
    assert_success(kvp(&with_dir(
        &dir,
        &[
            "write",
            "--append",
            "CLOUD_INIT|1785187982|finish|modules-final/config-scripts_user|0e5e179d-5341-478b-8456-fbb90621bdf8|e5f01809-a7a3-4279-aa64-1f18e21eda6e",
            r#"{"name":"modules-final/config-scripts_user","type":"finish","ts":"2026-07-27T21:33:24.339006+00:00","result":"SUCCESS","duration":0.5,"msg":"scripts ran"}"#,
        ],
    )));
    assert_success(kvp(&with_dir(
        &dir,
        &[
            "write",
            "--append",
            "CLOUD_INIT|1785187982|start|modules-final/config-keys_to_console|0e5e179d-5341-478b-8456-fbb90621bdf8|7792621b-b339-4274-8b71-2a3dcbd2db4e",
            r#"{"name":"modules-final/config-keys_to_console","type":"start","ts":"2026-07-27T21:33:24.344349+00:00","msg":"running keys_to_console"}"#,
        ],
    )));

    let entries = assert_json(kvp(&with_dir(&dir, &["dump", "--parse"])));
    assert_eq!(
        entries[0],
        json!({
            "type": "diagnostic", "kind": "finish", "agent": "CLOUD_INIT",
            "name": "modules-final/config-scripts_user", "vm_id": VM_ID,
            "event_id": "e5f01809-a7a3-4279-aa64-1f18e21eda6e",
            "timestamp": "2026-07-27T21:33:24.339Z", "encoding": "none",
            "result": "success", "duration": 500, "payload": "scripts ran",
        })
    );
    assert_eq!(entries[1]["kind"], "start");
    assert!(entries[1].get("result").is_none());
    assert!(entries[1].get("duration").is_none());

    let out =
        assert_success(kvp(&with_dir(&dir, &["dump", "--parse", "--text"])));
    assert!(out.contains("diagnostic kind=finish"));
    assert!(out.contains("agent=CLOUD_INIT"));
    assert!(!out.contains("boot_epoch"));
    assert!(out.contains("name=modules-final/config-scripts_user"));
    assert!(out.contains("vm_id=0e5e179d-5341-478b-8456-fbb90621bdf8"));
    assert!(out.contains("result=success"));
    assert!(out.contains("timestamp=2026-07-27T21:33:24.339Z"));
    assert!(out.contains("duration=500ms"));
    assert!(out.contains("payload=scripts ran"));
    let start = out.lines().nth(1).unwrap();
    assert!(start.contains("diagnostic kind=start"));
    assert!(!start.contains("result="));
    assert!(!start.contains("duration="));
}

#[test]
fn parsed_dump_renders_bytes_reports_and_raw_errors() {
    let dir = TempDir::new().unwrap();
    let store = store_at(&dir);
    DiagnosticWriter::new(store.clone(), "agent", VM_ID)
        .unwrap()
        .emit_event(
            "artifact",
            vec![0, 255],
            Some(Encoding::GzB64),
            Some(Outcome::Failure),
            Some(7),
        )
        .unwrap();
    store.append("note", "raw value").unwrap();
    store.append("DIAG_V1|bad", "junk").unwrap();
    assert_success(kvp(&with_dir(
        &dir,
        &["report-failure", "--vm-id", VM_ID, "--reason", "bad input"],
    )));
    let out =
        assert_success(kvp(&with_dir(&dir, &["dump", "--parse", "--text"])));
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0]
        .contains("encoding=gz+b64 result=fail duration=7ms payload_b64=AP8="));
    assert_eq!(
        lines[1],
        format!(
            "PROVISIONING_REPORT={}",
            store.read(PROVISIONING_REPORT_KEY).unwrap().unwrap()
        )
    );
    assert_eq!(lines[2], "raw key=note value=raw value");
    assert_eq!(lines[3], "raw key=DIAG_V1|bad value=junk error=malformed diagnostic or provisioning report");

    let entries =
        assert_json(kvp(&with_dir(&dir, &["dump", "--parse", "--json"])));
    assert_eq!(
        entries[0]["payload"],
        json!({"type": "bytes", "encoding": "base64", "data": "AP8="})
    );
    assert_eq!(entries[1]["reason"], "bad input");
    assert_eq!(entries[3]["error"], "malformed");
}

#[test]
fn dump_name_requires_parse() {
    let output = kvp(&["dump", "--name", "ssh"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("--parse"));
}

#[test]
fn conflicting_output_flags_fail_before_pool_access() {
    let dir = TempDir::new().unwrap();
    let output = kvp(&with_dir(&dir, &["--json", "dump", "--text"]));
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("--json and --text cannot be used together"));
    assert!(!store_at(&dir).path().exists());
}

#[test]
fn removed_diagnostic_options_are_rejected() {
    let cases: &[(&[&str], &str)] = &[
        (&["dump", "--parse-diagnostics"], "--parse-diagnostics"),
        (&["dump", "--parse", "--tail"], "--tail"),
        (&["dump", "--parse", "-n", "1"], "-n"),
        (&["emit", "--prefix", "agent"], "--prefix"),
    ];
    for (args, flag) in cases {
        let output = kvp(args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains(&format!("unexpected argument '{flag}'")));
    }
}

#[test]
fn dumps_fail_without_partial_output_for_invalid_physical_utf8() {
    let dir = TempDir::new().unwrap();
    let store = store_at(&dir);
    store
        .append_multiple([("good", "ok"), ("bad", "value")])
        .unwrap();
    let mut bytes = fs::read(store.path()).unwrap();
    let record_size = bytes.len() / 2;
    bytes[record_size] = 0xff;
    fs::write(store.path(), &bytes).unwrap();

    for args in [
        &["dump"][..],
        &["dump", "--parse"],
        &["dump", "--parse", "--text"],
    ] {
        let output = kvp(&with_dir(&dir, args));
        assert_eq!(output.status.code(), Some(3));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
}

#[rstest]
#[case::default_agent(None)]
#[case::custom_agent(Some("azure-init-test"))]
fn emit_writes_event_readable_by_dump(#[case] agent: Option<&str>) {
    let dir = TempDir::new().unwrap();
    let mut args = vec![
        "emit",
        "--name",
        "user:create_user",
        "--message",
        "created azureuser",
        "--vm-id",
        VM_ID,
    ];
    if let Some(agent) = agent {
        args.extend(["--agent", agent]);
    }
    assert_success(kvp(&with_dir(&dir, &args)));

    let entries = assert_json(kvp(&with_dir(&dir, &["dump", "--parse"])));
    let expected_agent = agent
        .unwrap_or(concat!("libazureinit-kvp/", env!("CARGO_PKG_VERSION")));
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["type"], "diagnostic");
    assert_eq!(entries[0]["kind"], "event");
    assert_eq!(entries[0]["agent"], expected_agent);
    assert_eq!(entries[0]["vm_id"], VM_ID);
    assert_eq!(entries[0]["name"], "user:create_user");
    assert_eq!(entries[0]["payload"], "created azureuser");
    assert_eq!(entries[0]["encoding"], "none");
    assert!(entries[0].get("result").is_none());
    assert!(entries[0].get("duration").is_none());
    let event_id =
        uuid::Uuid::parse_str(entries[0]["event_id"].as_str().unwrap())
            .unwrap();
    assert_eq!(event_id.get_version_num(), 4);

    let raw = assert_json(kvp(&with_dir(&dir, &["dump"])));
    let key = raw[0]["key"].as_str().unwrap();
    assert!(
        key.starts_with(&format!("DIAG_V1|{expected_agent}|{VM_ID}|event|"))
    );
    assert!(key.ends_with("|none|||0"));
}

#[test]
fn emit_rejects_invalid_uuid_without_creating_pool() {
    let dir = TempDir::new().unwrap();
    let output = kvp(&with_dir(
        &dir,
        &[
            "emit",
            "--name",
            "event",
            "--message",
            "test",
            "--vm-id",
            "vm-emit",
        ],
    ));
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr).unwrap().contains("UUID"));
    assert!(!store_at(&dir).path().exists());
}
