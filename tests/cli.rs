use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

use std::fs::{self, File};
use std::io::Write;
use tempfile::tempdir;

// Assert help text includes the --groups flag
#[test]
fn help_groups() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("azure-init")?;
    command.arg("--help");
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("-g, --groups <GROUPS>"));

    Ok(())
}

// Help output should mention new subcommands
#[test]
fn help_shows_new_subcommands() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("azure-init")?;
    command.arg("--help");
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("provision"))
        .stdout(predicate::str::contains("report"))
        .stdout(predicate::str::contains("status"));

    Ok(())
}

fn write_config_with_data_dir(
    data_dir: &std::path::Path,
) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let temp_dir = tempdir()?;
    let config_contents = format!(
        r#"
        [azure_init_data_dir]
        path = "{}"
        "#,
        data_dir.display(),
    );
    let config_path = temp_dir.path().join("azure-init-config.toml");
    fs::write(&config_path, config_contents)?;
    // Store path to reuse via symlink for tests
    std::os::unix::fs::symlink(&config_path, temp_dir.path().join("config"))
        .ok();
    Ok(temp_dir)
}

fn config_path_from_temp(temp_dir: &tempfile::TempDir) -> std::path::PathBuf {
    temp_dir.path().join("config")
}

// Status should be NotReady when no markers exist
#[test]
fn status_not_ready_without_markers() -> Result<(), Box<dyn std::error::Error>>
{
    let base = tempdir()?;
    let data_dir = base.path().join("data");
    fs::create_dir_all(&data_dir)?;

    let cfg_tmp = write_config_with_data_dir(&data_dir)?;
    let cfg_path = config_path_from_temp(&cfg_tmp);

    let mut cmd = Command::cargo_bin("azure-init")?;
    cmd.args(["--config", cfg_path.to_str().unwrap(), "status"]);
    cmd.assert()
        .failure()
        .stdout(predicate::str::contains("NotReady"));

    Ok(())
}

// Status should be Ready when the .provisioned marker exists
#[test]
fn status_ready_with_provisioned_marker(
) -> Result<(), Box<dyn std::error::Error>> {
    let base = tempdir()?;
    let data_dir = base.path().join("data");
    fs::create_dir_all(&data_dir)?;

    // main() falls back to this VM ID when it cannot read the system VM ID
    let fallback_vm_id = "00000000-0000-0000-0000-000000000000";
    let provisioned = data_dir.join(format!("{}.provisioned", fallback_vm_id));
    File::create(&provisioned)?;

    let cfg_tmp = write_config_with_data_dir(&data_dir)?;
    let cfg_path = config_path_from_temp(&cfg_tmp);

    let mut cmd = Command::cargo_bin("azure-init")?;
    cmd.args(["--config", cfg_path.to_str().unwrap(), "status"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Ready"));

    Ok(())
}

// Status should be Failed when the .failure marker exists and no .provisioned
#[test]
fn status_failed_with_failure_marker() -> Result<(), Box<dyn std::error::Error>>
{
    let base = tempdir()?;
    let data_dir = base.path().join("data");
    fs::create_dir_all(&data_dir)?;

    let fallback_vm_id = "00000000-0000-0000-0000-000000000000";
    let failure = data_dir.join(format!("{}.failure", fallback_vm_id));
    let mut f = File::create(&failure)?;
    writeln!(f, "result=failure|details=test")?;

    let cfg_tmp = write_config_with_data_dir(&data_dir)?;
    let cfg_path = config_path_from_temp(&cfg_tmp);

    let mut cmd = Command::cargo_bin("azure-init")?;
    cmd.args(["--config", cfg_path.to_str().unwrap(), "status"]);
    cmd.assert()
        .failure()
        .stdout(predicate::str::contains("Failed"));

    Ok(())
}

// Ensure no password-related flags are exposed by the CLI
#[test]
fn help_has_no_password_flags() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("azure-init")?;
    command.arg("--help");
    command
        .assert()
        .success()
        .stdout(predicate::str::is_match("(?i)password").unwrap().not());

    Ok(())
}

// Assert that the --version flag works and outputs the version
#[test]
fn version_flag() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("azure-init")?;
    command.arg("--version");
    command
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));

    Ok(())
}

// Assert that the -V flag works and outputs the version
#[test]
fn version_flag_short() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("azure-init")?;
    command.arg("-V");
    command
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));

    Ok(())
}

// Helper function to set up the log and provision files for cleaning
fn setup_clean_test() -> Result<
    (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ),
    Box<dyn std::error::Error>,
> {
    let temp_dir = tempdir()?;
    let data_dir = temp_dir.path().join("data");
    let log_file = temp_dir.path().join("azure-init.log");
    fs::create_dir_all(&data_dir)?;

    let provisioned_file = data_dir.join("vm-id.provisioned");
    File::create(provisioned_file)?;

    let mut log = File::create(&log_file)?;
    writeln!(log, "fake log line")?;

    let config_contents = format!(
        r#"
        [azure_init_data_dir]
        path = "{}"

        [azure_init_log_path]
        path = "{}"
        "#,
        data_dir.display(),
        log_file.display()
    );
    let config_path = temp_dir.path().join("azure-init-config.toml");
    fs::write(&config_path, config_contents)?;

    Ok((temp_dir, data_dir, log_file, config_path))
}

// Ensures that the `clean` command removes only the provisioned file
#[test]
fn clean_removes_only_provision_files_without_log_arg(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_temp_dir, _data_dir, log_file, config_path) = setup_clean_test()?;
    let provisioned_file = _data_dir.join("vm-id.provisioned");

    assert!(
        provisioned_file.exists(),
        ".provisioned file should exist before cleaning"
    );
    assert!(log_file.exists(), "log file should exist before cleaning");

    let mut cmd = Command::cargo_bin("azure-init")?;
    cmd.args(["--config", config_path.to_str().unwrap(), "clean"]);

    cmd.assert().success();

    assert!(
        !provisioned_file.exists(),
        "Expected .provisioned file to be deleted"
    );
    assert!(log_file.exists(), "log file should exist after cleaning");

    Ok(())
}

// Ensures that the `clean` command with the --logs arg
// removes both the provisioned file and the log file
#[test]
fn clean_removes_provision_and_log_files_with_log_arg(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_temp_dir, _data_dir, log_file, config_path) = setup_clean_test()?;
    let provisioned_file = _data_dir.join("vm-id.provisioned");

    assert!(
        provisioned_file.exists(),
        ".provisioned file should exist before cleaning"
    );
    assert!(log_file.exists(), "log file should exist before cleaning");

    let mut cmd = Command::cargo_bin("azure-init")?;
    cmd.args(["--config", config_path.to_str().unwrap(), "clean", "--logs"]);

    cmd.assert().success();

    assert!(
        !provisioned_file.exists(),
        "Expected .provisioned file to be deleted"
    );
    assert!(
        !log_file.exists(),
        "Expected azure-init.log file to be deleted"
    );

    Ok(())
}
