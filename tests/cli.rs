//! The command line itself, run as a separate process. None of these tests
//! need the sandbox servers.

use std::{fs, process::Command};

fn vault_mirror() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault-mirror"));
    command.env_clear();
    command
}

#[test]
fn snapshots_need_only_the_backup_directory() {
    let dir = tempfile::tempdir().unwrap();
    let output = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .env(
            "SOURCE_PASSWORD_FILE",
            dir.path().join("unavailable-source-secret"),
        )
        .env(
            "BACKUP_PASSPHRASE_FILE",
            dir.path().join("unavailable-backup-secret"),
        )
        .arg("snapshots")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("No snapshots"));
    assert!(!dir.path().join("key.age").exists());
}

#[test]
fn export_uses_an_identity_without_requiring_vault_credentials() {
    use age::secrecy::ExposeSecret;
    use serde_json::json;
    use vault_mirror::store::{Keys, Protection, Store, VaultOrigin};

    let dir = tempfile::tempdir().unwrap();
    let identity = age::x25519::Identity::generate();
    let path = dir.path().join("identity.txt");
    fs::write(&path, identity.to_string().expose_secret()).unwrap();
    let keys = Keys::open(
        dir.path(),
        &Protection::Recipients(vec![identity.to_public().to_string()]),
    )
    .unwrap();
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let mut document = json!({"encrypted":false,"folders":[],"items":[{"id":"item-id","type":2,"name":"synthetic item","notes":"synthetic note","fields":[{"name":"vault-mirror","value":format!("source-item {} - -", "ab".repeat(32)),"type":1},{"name":"vault-mirror","value":"my own field","type":0}]}],"vaultMirror":{"format":2,"scope":"personal","plainStrings":{},"attachments":[],"markers":{}}});
    let origin = VaultOrigin {
        api_url: "http://127.0.0.1".into(),
        account_id: "vault".into(),
        organization_id: None,
    };
    let writer = store
        .begin_snapshot(&origin, time::OffsetDateTime::now_utc())
        .unwrap();
    let vault_mirror::snapshot::AttachmentStorage::Snapshot {
        snapshot_id,
        ciphertext_sha256,
    } = writer
        .write_attachment("attachment-id", b"synthetic attachment bytes")
        .unwrap()
    else {
        panic!("a new snapshot owns its attachment");
    };
    document["vaultMirror"]["attachments"] = json!([{"itemId":"item-id","attachmentId":"attachment-id","fileName":"../recovered.txt","size":26,"stored":true,"snapshotId":snapshot_id,"ciphertextSha256":ciphertext_sha256}]);
    writer.finish(document).unwrap();
    let output_dir = dir.path().join("recovered");
    let command = |output_dir: &std::path::Path| {
        let mut command = vault_mirror();
        command
            .env("BACKUPS_DIR", dir.path())
            .env(
                "SOURCE_PASSWORD_FILE",
                dir.path().join("unavailable-source-secret"),
            )
            .env(
                "BACKUP_PASSPHRASE_FILE",
                dir.path().join("unavailable-backup-secret"),
            )
            .arg("export")
            .arg("--identity")
            .arg(&path)
            .arg("--output")
            .arg(output_dir);
        command
    };
    let lock = vault_mirror::app::lock_backups_dir(dir.path()).unwrap();
    let locked = command(&output_dir).output().unwrap();
    assert!(!locked.status.success());
    assert!(String::from_utf8_lossy(&locked.stderr).contains("another vault-mirror run"));
    assert!(!output_dir.exists());
    drop(lock);
    let output = command(&output_dir).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported: serde_json::Value =
        serde_json::from_slice(&fs::read(output_dir.join("export.json")).unwrap()).unwrap();
    assert_eq!(exported["items"][0]["notes"], "synthetic note");
    assert_eq!(
        exported["items"][0]["fields"],
        json!([{"name":"vault-mirror","value":"my own field","type":0}])
    );
    assert_eq!(
        fs::read(output_dir.join("attachments/item-id/attachment-id/recovered.txt")).unwrap(),
        b"synthetic attachment bytes"
    );
    assert!(!dir.path().join("key.age").exists());

    for (role, prefix, unrelated) in [("dest", "DEST", "SOURCE"), ("source", "SOURCE", "DEST")] {
        let output = vault_mirror()
            .env("BACKUPS_DIR", dir.path())
            .env(
                format!("{unrelated}_PASSWORD_FILE"),
                dir.path().join("unavailable-unrelated-secret"),
            )
            .env(format!("{prefix}_SERVER"), "invalid-server-url")
            .env(format!("{prefix}_CLIENT_ID"), "user.synthetic")
            .env(format!("{prefix}_CLIENT_SECRET"), "synthetic-api-secret")
            .env(format!("{prefix}_PASSWORD"), "synthetic-password")
            .arg("restore")
            .arg("--to")
            .arg(role)
            .arg("--identity")
            .arg(&path)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("invalid server URL"), "{stderr}");
        assert!(!stderr.contains("unavailable-unrelated-secret"), "{stderr}");
        assert!(!dir.path().join("restore-hold.json").exists());
    }

    fs::write(
        dir.path().join(format!(
            "source/attachments/{snapshot_id}/attachment-id.age"
        )),
        b"damaged encrypted attachment",
    )
    .unwrap();
    let damaged_output = dir.path().join("damaged-recovery");
    let output = command(&damaged_output).output().unwrap();
    assert!(!output.status.success());
    assert!(!damaged_output.exists());
}

#[test]
fn a_command_is_required() {
    let output = vault_mirror().output().unwrap();
    assert!(!output.status.success());
    let help = String::from_utf8_lossy(&output.stderr);
    assert!(help.contains("Usage:"), "{help}");
}

#[test]
fn an_excessive_healthcheck_age_is_reported_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("last-run.json"),
        serde_json::to_vec(&vault_mirror::app::Status {
            status: vault_mirror::app::RunResult::Success,
            finished_at: "2026-10-16T12:00:00Z".into(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let output = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .arg("healthcheck")
        .arg("--max-age-hours")
        .arg(u64::MAX.to_string())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("--max-age-hours is too large"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
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

#[cfg(unix)]
#[test]
fn a_variable_that_is_not_utf8_does_not_crash_the_command() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    let dir = tempfile::tempdir().unwrap();
    let output = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .env("UNRELATED", OsStr::from_bytes(b"caf\xe9"))
        .arg("healthcheck")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(output.status.code(), Some(1), "{stderr}");
}
