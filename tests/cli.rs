//! The command line itself, run as a separate process. None of these tests
//! need the sandbox servers.

use std::{fs, process::Command};

use vault_mirror::app::{self, BackupAccess};

fn vault_mirror() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault-mirror"));
    command.env_clear();
    // Windows can't open network sockets without it.
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
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
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No snapshots"));
    assert!(dir.path().join("run.lock").is_file());
    assert!(!dir.path().join("key.age").exists());
}

#[test]
fn backup_directory_precedence_includes_the_image_default() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("vault-mirror.env");
    for (case, file, explicit, expected) in [
        ("native", None, None, "backups"),
        ("image", None, None, "image-backups"),
        ("file", Some("file-backups"), None, "file-backups"),
        (
            "override",
            Some("file-backups"),
            Some("override-backups"),
            "override-backups",
        ),
    ] {
        let working = dir.path().join(case);
        fs::create_dir(&working).unwrap();
        for path in [
            "backups",
            "image-backups",
            "file-backups",
            "override-backups",
        ] {
            fs::create_dir(working.join(path)).unwrap();
        }
        let text = format!(
            "SOURCE_PASSWORD_FILE=/unavailable-secret\nBACKUP_PASSPHRASE_FILE=/unavailable-secret\n{}",
            file.map(|path| format!("BACKUPS_DIR={path}\n"))
                .unwrap_or_default()
        );
        fs::write(&settings, text).unwrap();
        let mut command = vault_mirror();
        command
            .current_dir(&working)
            .arg("--env-file")
            .arg(&settings)
            .arg("snapshots");
        if case != "native" {
            command.env("VAULT_MIRROR_DEFAULT_BACKUPS_DIR", "image-backups");
        }
        if let Some(path) = explicit {
            command.env("BACKUPS_DIR", path);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            working.join(expected).join("run.lock").is_file(),
            "{case}: wrong backup directory"
        );
        assert!(!working.join(expected).join("key.age").exists());
    }
}

#[test]
fn an_export_needs_only_the_backup_passphrase() {
    use serde_json::json;
    use vault_mirror::store::{Keys, Store};

    let dir = tempfile::tempdir().unwrap();
    let passphrase = "synthetic 'quoted' \"passphrase\" $literal";
    let keys = Keys::open(dir.path(), passphrase).unwrap();
    let store = Store::new(dir.path(), "source", &keys);
    let writer = store.begin(time::OffsetDateTime::now_utc()).unwrap();
    writer
        .write_attachment("attachment-id", b"synthetic attachment bytes")
        .unwrap();
    let id = writer
        .finish(&json!({"encrypted":false,"folders":[],"items":[{"id":"item-id","type":2,"name":"synthetic item","notes":"synthetic note"}],"vaultMirror":{"scope":"personal","plainStrings":{},"attachments":[{"itemId":"item-id","attachmentId":"attachment-id","fileName":"../recovered.txt"}]}}))
        .unwrap();
    let output_dir = dir.path().join("recovered");
    let settings = dir.path().join("vault-mirror.env");
    fs::write(
        &settings,
        format!("SOURCE_PASSWORD_FILE=/unavailable-secret\nBACKUP_PASSPHRASE={passphrase}\n"),
    )
    .unwrap();
    let command = |output_dir: &std::path::Path| {
        let mut command = vault_mirror();
        command
            .env("BACKUPS_DIR", dir.path())
            .arg("--env-file")
            .arg(&settings)
            .arg("export")
            .arg("--output")
            .arg(output_dir);
        command
    };
    let lock = app::lock_backups_dir(dir.path(), BackupAccess::Write).unwrap();
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
        fs::read(output_dir.join("attachments/attachment-id/attachment")).unwrap(),
        b"synthetic attachment bytes"
    );

    fs::write(
        dir.path()
            .join(format!("source/{id}/attachments/attachment-id.age")),
        b"damaged encrypted attachment",
    )
    .unwrap();
    let damaged_output = dir.path().join("damaged-recovery");
    let output = command(&damaged_output).output().unwrap();
    assert!(!output.status.success());
    assert!(!damaged_output.exists());
}

fn recovery_backup(backups: &std::path::Path, names: &[String]) -> serde_json::Value {
    use serde_json::json;
    use vault_mirror::store::{Keys, Store};

    let keys = Keys::open(backups, "synthetic passphrase").unwrap();
    let store = Store::new(backups, "source", &keys);
    let writer = store.begin(time::OffsetDateTime::now_utc()).unwrap();
    let attachments: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let id = format!("attachment-{index}");
            writer
                .write_attachment(&id, format!("synthetic bytes {index}").as_bytes())
                .unwrap();
            json!({"itemId":"item-id","attachmentId":id,"fileName":name})
        })
        .collect();
    let document = json!({
        "encrypted": false,
        "folders": [],
        "items": [{"id":"item-id","type":2,"name":"synthetic item","notes":"synthetic note"}],
        "vaultMirror": {"scope":"personal","plainStrings":{},"attachments":attachments},
    });
    writer.finish(&document).unwrap();
    document
}

fn export_backup(backups: &std::path::Path, output: &std::path::Path) -> std::process::Output {
    vault_mirror()
        .env("BACKUPS_DIR", backups)
        .env("BACKUP_PASSPHRASE", "synthetic passphrase")
        .args(["export", "--output"])
        .arg(output)
        .output()
        .unwrap()
}

#[test]
fn attachment_exports_use_fixed_paths_and_preserve_original_names() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = [
        "report<>:\"|?*.txt",
        "CON",
        "COM1.txt",
        "LPT¹.txt",
        "control\0\u{1f}.txt",
        "trailing.",
        "trailing ",
        "../outside.txt",
        "..\\outside.txt",
        ".",
        "..",
        "duplicate.txt",
        "duplicate.txt",
    ]
    .into_iter()
    .map(str::to_string)
    .chain(["long".repeat(100)])
    .collect();
    let original = recovery_backup(dir.path(), &names);
    let output_dir = dir.path().join("recovered");
    let output = export_backup(dir.path(), &output_dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported: serde_json::Value =
        serde_json::from_slice(&fs::read(output_dir.join("export.json")).unwrap()).unwrap();
    assert_eq!(exported, original);
    assert_eq!(
        fs::read_dir(output_dir.join("attachments"))
            .unwrap()
            .count(),
        names.len()
    );
    for index in 0..names.len() {
        let directory = output_dir.join(format!("attachments/attachment-{index}"));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(
            fs::read(directory.join("attachment")).unwrap(),
            format!("synthetic bytes {index}").as_bytes()
        );
    }
    assert!(!dir.path().join("outside.txt").exists());
    assert!(!output_dir.join("outside.txt").exists());
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!printed.contains("synthetic item"), "{printed}");
    assert!(!printed.contains("synthetic note"), "{printed}");
    assert!(!printed.contains("synthetic bytes"), "{printed}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_export_warns_about_plaintext_left_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    recovery_backup(&backups, &["original.txt".into()]);

    // export.json fits Linux's path limit, but the attachment directory does
    // not, so the failure happens after the plaintext vault has been written.
    let mut recovered = dir.path().to_path_buf();
    while recovered.as_os_str().len() < 4075 {
        let length = (4075 - recovered.as_os_str().len() - 1).min(100);
        if length == 0 {
            recovered.as_mut_os_string().push("x");
        } else {
            recovered.push("x".repeat(length));
        }
    }
    let output = export_backup(&backups, &recovered);
    assert_eq!(output.status.code(), Some(1));
    assert!(recovered.join("export.json").is_file());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unencrypted vault data"),
        "missing plaintext warning"
    );
    assert!(
        stderr.contains("unencrypted files may remain"),
        "missing partial-export guidance"
    );
    assert!(
        stderr.contains("new --output directory"),
        "missing retry guidance"
    );
    assert!(
        !stderr.contains("synthetic note"),
        "export leaked vault content"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn an_export_parent_error_names_the_path_without_writing_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    recovery_backup(&backups, &[]);
    let parent = dir.path().join("not-a-directory");
    fs::write(&parent, b"keep this file").unwrap();
    let recovered = parent.join("recovered");
    let output = export_backup(&backups, &recovered);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!("creating {}", parent.display())),
        "{stderr}"
    );
    assert!(stderr.contains("--output"), "{stderr}");
    assert!(!stderr.contains("unencrypted vault data"), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(!recovered.exists());
    assert_eq!(fs::read(parent).unwrap(), b"keep this file");
}

#[test]
fn recovery_readers_share_the_lock_and_exclude_runs() {
    let dir = tempfile::tempdir().unwrap();
    recovery_backup(dir.path(), &["original.txt".into()]);
    fs::write(dir.path().join("run.lock"), b"retained lock").unwrap();
    let reader = app::lock_backups_dir(dir.path(), BackupAccess::Read).unwrap();

    let listed = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .arg("snapshots")
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let exported = export_backup(dir.path(), &dir.path().join("recovered"));
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );

    let settings = unreachable_settings(dir.path(), "");
    let run = vault_mirror()
        .arg("--env-file")
        .arg(settings)
        .arg("run")
        .output()
        .unwrap();
    assert!(!run.status.success());
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(printed.contains("another vault-mirror run"), "{printed}");
    assert!(!dir.path().join("last-run.json").exists());
    // Another test's child can inherit this descriptor until exec.
    reader.unlock().unwrap();
    drop(reader);
    let writer = app::lock_backups_dir(dir.path(), BackupAccess::Write).unwrap();
    drop(writer);
    assert_eq!(
        fs::read(dir.path().join("run.lock")).unwrap(),
        b"retained lock"
    );
}

#[test]
fn snapshots_refuse_an_active_writer() {
    let dir = tempfile::tempdir().unwrap();
    let writer = app::lock_backups_dir(dir.path(), BackupAccess::Write).unwrap();
    let output = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .arg("snapshots")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another vault-mirror run"));
    drop(writer);
}

#[test]
fn recovery_commands_explain_a_missing_backup_directory() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("missing-backups");
    let recovered = dir.path().join("recovered");
    for command in ["snapshots", "export"] {
        let mut process = vault_mirror();
        process.env("BACKUPS_DIR", &backups).arg(command);
        if command == "export" {
            process.arg("--output").arg(&recovered);
        }
        let output = process.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("no backup directory"), "{stderr}");
        assert!(stderr.contains(&backups.display().to_string()), "{stderr}");
        assert!(
            stderr.contains("BACKUPS_DIR") && stderr.contains("--env-file"),
            "{stderr}"
        );
        assert!(!stderr.contains("retain run.lock"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
    assert!(!backups.exists());
    assert!(!recovered.exists());
}

#[cfg(unix)]
fn permission_tests_available() -> bool {
    let uid = Command::new("id").arg("-u").output().unwrap();
    assert!(uid.status.success());
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        eprintln!("skipping Unix permission test: root bypasses file permissions");
        return false;
    }
    true
}

#[cfg(unix)]
struct RestorePermissions(Vec<(std::path::PathBuf, fs::Permissions)>);

#[cfg(unix)]
impl RestorePermissions {
    fn read_only(path: &std::path::Path, lock: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let mut restore = Self(vec![(
            path.to_path_buf(),
            fs::metadata(path).unwrap().permissions(),
        )]);
        if lock {
            let file = path.join("run.lock");
            restore
                .0
                .push((file.clone(), fs::metadata(&file).unwrap().permissions()));
            fs::set_permissions(file, fs::Permissions::from_mode(0o400)).unwrap();
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
        restore
    }
}

#[cfg(unix)]
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        for (path, permissions) in &self.0 {
            fs::set_permissions(path, permissions.clone()).unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn recovery_reads_an_existing_lock_without_changing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    if !permission_tests_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    let original = recovery_backup(&backups, &["original.txt".into()]);
    fs::write(backups.join("run.lock"), b"retained lock").unwrap();
    let _restore = RestorePermissions::read_only(&backups, true);
    let listed = vault_mirror()
        .env("BACKUPS_DIR", &backups)
        .arg("snapshots")
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let recovered = dir.path().join("recovered");
    let exported = export_backup(&backups, &recovered);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(recovered.join("export.json")).unwrap()
        )
        .unwrap(),
        original
    );
    assert_eq!(
        fs::read(recovered.join("attachments/attachment-0/attachment")).unwrap(),
        b"synthetic bytes 0"
    );
    assert_eq!(
        fs::metadata(&backups).unwrap().permissions().mode() & 0o777,
        0o500
    );
    assert_eq!(
        fs::metadata(backups.join("run.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    assert_eq!(
        fs::read(backups.join("run.lock")).unwrap(),
        b"retained lock"
    );
}

#[cfg(unix)]
#[test]
fn recovery_refuses_a_missing_lock_in_a_read_only_directory() {
    use std::os::unix::fs::PermissionsExt;

    if !permission_tests_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    recovery_backup(&backups, &["original.txt".into()]);
    let _restore = RestorePermissions::read_only(&backups, false);
    let listed = vault_mirror()
        .env("BACKUPS_DIR", &backups)
        .arg("snapshots")
        .output()
        .unwrap();
    let recovered = dir.path().join("recovered");
    let exported = export_backup(&backups, &recovered);
    for output in [listed, exported] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("run.lock"), "{stderr}");
        assert!(
            stderr.contains("copy") && stderr.contains("writable") && stderr.contains("retain"),
            "{stderr}"
        );
    }
    assert!(!backups.join("run.lock").exists());
    assert!(!recovered.exists());
    assert_eq!(
        fs::metadata(&backups).unwrap().permissions().mode() & 0o777,
        0o500
    );
}

#[test]
fn a_command_is_required() {
    let output = vault_mirror().output().unwrap();
    assert!(!output.status.success());
    let help = String::from_utf8_lossy(&output.stderr);
    assert!(help.contains("Usage:"), "{help}");
}

#[test]
fn missing_source_settings_point_to_setup_and_the_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("vault-mirror.env"), "# existing settings\n").unwrap();
    for command in ["run", "check", "daemon"] {
        let output = vault_mirror()
            .current_dir(dir.path())
            .arg(command)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("no source vault configured"), "{stderr}");
        assert!(stderr.contains("vault-mirror init"), "{stderr}");
        assert!(stderr.contains("--env-file vault-mirror.env"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
    assert!(!dir.path().join("backups").exists());
}

#[test]
fn rejected_server_urls_do_not_expose_secrets_in_run_diagnostics() {
    let secret = "synthetic-url-secret";
    for (server, reason) in [
        (
            format!("ftp://127.0.0.1/?token={secret}"),
            "expected an http or https URL",
        ),
        (
            format!("127.0.0.1/?token={secret}"),
            "relative URL without a base",
        ),
        (
            format!("http://{secret}:{secret}@127.0.0.1:invalid/"),
            "invalid port number",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = vault_mirror()
            .env("BACKUPS_DIR", dir.path())
            .env("BACKUP_PASSPHRASE", "synthetic passphrase")
            .env("SOURCE_SERVER", &server)
            .env("SOURCE_CLIENT_ID", "user.synthetic")
            .env("SOURCE_CLIENT_SECRET", "synthetic secret")
            .env("SOURCE_PASSWORD", "synthetic password")
            .arg("run")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let persisted = fs::read_to_string(vault_mirror::app::status_path(dir.path())).unwrap();
        let status = vault_mirror::app::read_status(dir.path()).unwrap();
        let notification = vault_mirror::app::describe(&status);
        for diagnostic in [stdout.as_ref(), stderr.as_ref(), &persisted, &notification] {
            assert!(!diagnostic.contains(secret), "a URL secret was exposed");
            assert!(
                !diagnostic.contains(&server),
                "the rejected URL was exposed"
            );
        }
        assert!(!status.succeeded());
        assert_eq!(status.failure_stage.as_deref(), Some("source login"));
        let error = format!("invalid server URL: {reason}");
        // Do not print diagnostics on failure: they may contain the secret.
        assert!(status.error.as_deref() == Some(error.as_str()));
        assert!(stderr.contains(&format!("during source login: {error}")));
        assert!(!stdout.contains("ERROR"));
        assert!(notification == format!("vault-mirror FAILED; during source login: {error}"));
    }
}

#[test]
fn a_missing_ca_file_does_not_suppress_the_failure_notification() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (received, notifications) = mpsc::channel();
    let server = thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut length = 0;
        let mut header = String::new();
        while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
            header.clear();
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_ok() {
            let _ = received.send((request_line, String::from_utf8_lossy(&body).into_owned()));
        }
        let _ = reader
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    });

    let dir = tempfile::tempdir().unwrap();
    let token = "synthetic-notify-token";
    let output = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .env("BACKUP_PASSPHRASE", "synthetic passphrase")
        .env("SOURCE_SERVER", "http://127.0.0.1:9")
        .env("SOURCE_CLIENT_ID", "user.synthetic")
        .env("SOURCE_CLIENT_SECRET", "synthetic secret")
        .env("SOURCE_PASSWORD", "synthetic password")
        .env("CA_CERT_FILE", dir.path().join("missing.pem"))
        .env(
            "NOTIFY_FAILURE_URL",
            format!("http://{address}/notify?token={token}"),
        )
        .arg("run")
        .output()
        .unwrap();
    // Wakes the server if vault-mirror never connected, so it always ends.
    let _ = TcpStream::connect(address);
    server.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    let status = app::read_status(dir.path()).unwrap();
    assert_eq!(status.failure_stage.as_deref(), Some("configuration"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for diagnostic in [stdout.as_ref(), stderr.as_ref()] {
        for secret in [
            token,
            "synthetic passphrase",
            "synthetic secret",
            "synthetic password",
        ] {
            assert!(!diagnostic.contains(secret), "a secret was exposed");
        }
    }
    // The output was checked for secrets above.
    let (request_line, body) = notifications.try_recv().unwrap_or_else(|_| {
        panic!("no failure notification arrived; stdout:\n{stdout}\nstderr:\n{stderr}")
    });
    assert!(request_line.starts_with("POST /notify?token="));
    assert_eq!(body, app::describe(&status));
    assert!(body.starts_with("vault-mirror FAILED; during configuration: "));
    assert!(stderr.contains("standard certificate authorities"));
}

#[test]
fn healthcheck_reports_an_unrecorded_run_without_creating_files() {
    let dir = tempfile::tempdir().unwrap();
    for backups in [dir.path().to_path_buf(), dir.path().join("missing")] {
        let output = vault_mirror()
            .env("BACKUPS_DIR", &backups)
            .arg("healthcheck")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{stdout}");
        assert!(stdout.starts_with("unhealthy: no run recorded"), "{stdout}");
        assert!(
            stdout.contains("BACKUPS_DIR") && stdout.contains("--env-file"),
            "{stdout}"
        );
        assert!(output.stderr.is_empty());
        assert!(!backups.join("last-run.json").exists());
        assert!(!backups.join("run.lock").exists());
    }
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn healthcheck_keeps_a_damaged_status_file_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("last-run.json"), "not JSON").unwrap();
    let output = vault_mirror()
        .env("BACKUPS_DIR", dir.path())
        .arg("healthcheck")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("is not a status file"), "{stderr}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read(dir.path().join("last-run.json")).unwrap(),
        b"not JSON"
    );
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
            r#"{{"status":"success","started_at":"{finished}","finished_at":"{finished}","duration_seconds":1.0,"version":"test"}}"#
        ),
    )
    .unwrap();
    let settings = dir.path().join("vault-mirror.env");
    fs::write(&settings, format!("BACKUPS_DIR={}\n", backups.display())).unwrap();

    let output = vault_mirror()
        .current_dir(dir.path())
        .env(
            "VAULT_MIRROR_DEFAULT_BACKUPS_DIR",
            dir.path().join("image-backups"),
        )
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

#[test]
fn a_misspelt_setting_in_the_settings_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("vault-mirror.env");
    for line in [
        "INCLUDE_FOLDER=Work",
        "VAULT_MIRROR_DEFAULT_BACKUPS_DIR=/data",
        "SCHEDULE_FILE=/run/secrets/schedule",
        "pass word=hunter2",
        // A secret that lost its line break, left of the `=` it holds.
        "syntheticSecret=remaining",
    ] {
        fs::write(&settings, format!("BACKUPS_DIR=backups\n{line}\n")).unwrap();
        let output = vault_mirror()
            .current_dir(dir.path())
            .arg("--env-file")
            .arg(&settings)
            .arg("snapshots")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{line}");
        assert!(
            stderr.contains("line 2 sets a name that is not a setting"),
            "{stderr}"
        );
        let (name, value) = line.split_once('=').unwrap();
        assert!(
            !stderr.contains(name) && !stderr.contains(value),
            "{stderr}"
        );
    }
}

/// A daemon process, killed when dropped so a failing test leaves none
/// behind.
#[cfg(unix)]
struct Daemon(std::process::Child);

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon whose source server answers each request with an error once
/// `answer` lets it, and the server's notice of each request it receives.
#[cfg(unix)]
fn daemon_with_server(
    dir: &std::path::Path,
) -> (
    Daemon,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<String>,
) {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        process::Stdio,
        sync::mpsc,
        thread,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (requested, requests) = mpsc::channel();
    let (answer, answers) = mpsc::channel::<()>();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.read(&mut [0; 4096]);
            let _ = requested.send(());
            if answers.recv().is_err() {
                return;
            }
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    let mut daemon = Daemon(
        vault_mirror()
            .env("BACKUPS_DIR", dir)
            .env("BACKUP_PASSPHRASE", "synthetic passphrase")
            .env("SOURCE_SERVER", format!("http://{address}"))
            .env("SOURCE_CLIENT_ID", "user.synthetic")
            .env("SOURCE_CLIENT_SECRET", "synthetic secret")
            .env("SOURCE_PASSWORD", "synthetic password")
            .env("SCHEDULE", "0 3 * * *")
            .arg("daemon")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (line, lines) = mpsc::channel();
    let stdout = daemon.0.stdout.take().unwrap();
    thread::spawn(move || {
        for text in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line.send(text);
        }
    });
    (daemon, requests, answer, lines)
}

#[cfg(unix)]
fn signal(daemon: &Daemon, name: &str) {
    let sent = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(daemon.0.id().to_string())
        .status()
        .unwrap();
    assert!(sent.success());
}

#[cfg(unix)]
fn stopped(mut daemon: Daemon, lines: std::sync::mpsc::Receiver<String>) -> (bool, Vec<String>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "the daemon did not stop: {:?}",
                lines.try_iter().collect::<Vec<_>>()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    (status.success(), lines.iter().collect())
}

#[cfg(unix)]
#[test]
fn the_daemon_runs_at_once_and_stops_after_a_run_it_was_interrupted_in() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, requests, answer, lines) = daemon_with_server(dir.path());
    let timeout = std::time::Duration::from_secs(30);
    requests.recv_timeout(timeout).expect("a run at startup");
    signal(&daemon, "INT");
    answer.send(()).unwrap();
    let (succeeded, printed) = stopped(daemon, lines);
    assert!(succeeded, "{printed:?}");
    let printed = printed.join("\n");
    assert!(printed.contains("Stopping"), "{printed}");
    assert!(!printed.contains("Waiting"), "{printed}");
}

#[cfg(unix)]
#[test]
fn the_daemon_stops_between_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, requests, answer, lines) = daemon_with_server(dir.path());
    let timeout = std::time::Duration::from_secs(30);
    requests.recv_timeout(timeout).expect("a run at startup");
    answer.send(()).unwrap();
    loop {
        let line = lines.recv_timeout(timeout).expect("a line from the daemon");
        if line.contains("Waiting") {
            break;
        }
    }
    signal(&daemon, "TERM");
    let (succeeded, printed) = stopped(daemon, lines);
    assert!(succeeded, "{printed:?}");
    assert!(printed.join("\n").contains("Stopping"));
}

/// Settings with server addresses vault-mirror refuses before it connects,
/// and `extra` lines.
fn unreachable_settings(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
    let settings = dir.join("vault-mirror.env");
    let mut text = format!(
        "BACKUPS_DIR={}\nBACKUP_PASSPHRASE=synthetic passphrase\n",
        dir.display()
    );
    for side in ["SOURCE", "DEST"] {
        text.push_str(&format!(
            "{side}_SERVER=ftp://synthetic.invalid\n{side}_CLIENT_ID=user.synthetic\n\
             {side}_CLIENT_SECRET=synthetic secret\n{side}_PASSWORD=synthetic password\n"
        ));
    }
    text.push_str(extra);
    fs::write(&settings, text).unwrap();
    settings
}

#[test]
fn an_impossible_schedule_stops_before_the_first_run() {
    let dir = tempfile::tempdir().unwrap();
    let settings = unreachable_settings(dir.path(), "SCHEDULE=0 0 31 2 *\n");
    let output = vault_mirror()
        .arg("--env-file")
        .arg(settings)
        .arg("daemon")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("the cron schedule never runs again"));
    assert!(output.stdout.is_empty(), "the daemon attempted a run");
    for name in ["run.lock", "key.age", "state.json", "last-run.json"] {
        assert!(!dir.path().join(name).exists(), "created {name}");
    }
}

#[test]
fn invalid_settings_do_not_echo_their_values() {
    let secret = "synthetic-do-not-share";
    for setting in [
        "RETENTION_DAYS",
        "SKIP_ATTACHMENTS",
        "ALLOW_ORGANIZATION_METADATA_LOSS",
        "SCHEDULE",
        "TZ",
    ] {
        for from_environment in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let extra = if from_environment {
                "SCHEDULE=0 3 * * *\n".to_string()
            } else {
                format!("SCHEDULE=0 3 * * *\n{setting}={secret}\n")
            };
            let settings = unreachable_settings(dir.path(), &extra);
            let mut command = vault_mirror();
            if from_environment {
                command.env(setting, secret);
            }
            let output = command
                .arg("--env-file")
                .arg(settings)
                .arg("daemon")
                .output()
                .unwrap();
            assert!(!output.status.success());
            for diagnostic in [&output.stdout, &output.stderr] {
                assert!(
                    !String::from_utf8_lossy(diagnostic).contains(secret),
                    "a rejected setting value was exposed"
                );
            }
            assert!(String::from_utf8_lossy(&output.stderr).contains(setting));
            assert!(!dir.path().join("key.age").exists());
        }
    }
}

#[test]
fn organization_metadata_loss_can_be_allowed_for_run_and_check() {
    let dir = tempfile::tempdir().unwrap();
    let settings = unreachable_settings(dir.path(), "ALLOW_ORGANIZATION_METADATA_LOSS=false\n");
    for command in ["run", "check"] {
        let output = vault_mirror()
            .arg("--env-file")
            .arg(&settings)
            .arg(command)
            .arg("--allow-organization-metadata-loss")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            message.contains("expected an http or https URL"),
            "{message}"
        );
    }
    let output = vault_mirror()
        .env("ALLOW_ORGANIZATION_METADATA_LOSS", "yes")
        .arg("--env-file")
        .arg(&settings)
        .arg("check")
        .arg("--allow-organization-metadata-loss")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("configuration: ALLOW_ORGANIZATION_METADATA_LOSS has an invalid value")
    );
}

#[test]
fn attachments_can_be_skipped_from_the_settings_file_or_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let check = |extra: &str, env: Option<&str>| {
        let settings = unreachable_settings(dir.path(), extra);
        let mut command = vault_mirror();
        if let Some(value) = env {
            command.env("SKIP_ATTACHMENTS", value);
        }
        let output = command
            .arg("--env-file")
            .arg(&settings)
            .arg("check")
            .output()
            .unwrap();
        assert!(!output.status.success());
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let invalid = "configuration: SKIP_ATTACHMENTS has an invalid value";

    let past_settings = "expected an http or https URL";

    let stderr = check("SKIP_ATTACHMENTS=true\n", None);
    assert!(stderr.contains(past_settings), "{stderr}");
    assert!(check("SKIP_ATTACHMENTS=maybe\n", None).contains(invalid));
    assert!(check("SKIP_ATTACHMENTS=true\n", Some("maybe")).contains(invalid));
    let stderr = check("SKIP_ATTACHMENTS=maybe\n", Some("false"));
    assert!(stderr.contains(past_settings), "{stderr}");
}
