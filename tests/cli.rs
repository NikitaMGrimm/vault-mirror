//! The command line itself, run as a separate process. None of these tests
//! need the sandbox servers.

use std::{fs, process::Command};

fn vault_mirror() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault-mirror"));
    command.env_remove("BACKUPS_DIR");
    command
}

#[test]
fn a_command_is_required() {
    let output = vault_mirror().output().unwrap();
    assert!(!output.status.success());
    let help = String::from_utf8_lossy(&output.stderr);
    assert!(help.contains("Usage:"), "{help}");
}

#[test]
fn healthcheck_reads_the_backup_directory_from_the_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("elsewhere");
    fs::create_dir(&backups).unwrap();
    let finished = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    fs::write(
        backups.join("last-run.json"),
        format!(
            r#"{{"status":"success","dry_run":false,"started_at":"{finished}","finished_at":"{finished}","duration_seconds":1.0,"version":"test"}}"#
        ),
    )
    .unwrap();
    let settings = dir.path().join("vault-mirror.env");
    fs::write(&settings, format!("BACKUPS_DIR={}\n", backups.display())).unwrap();

    let output = vault_mirror()
        .current_dir(dir.path())
        .arg("--env-file")
        .arg(&settings)
        .arg("healthcheck")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("healthy"), "{stdout}");
}
