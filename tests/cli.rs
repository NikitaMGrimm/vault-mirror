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
fn an_export_needs_only_the_backup_passphrase() {
    use serde_json::json;
    use vault_mirror::store::{Keys, Store};

    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::open(dir.path(), "synthetic passphrase").unwrap();
    let store = Store::new(dir.path(), "source", &keys);
    let writer = store.begin(time::OffsetDateTime::now_utc()).unwrap();
    writer
        .write_attachment("attachment-id", b"synthetic attachment bytes")
        .unwrap();
    let id = writer
        .finish(&json!({"encrypted":false,"folders":[],"items":[{"id":"item-id","type":2,"name":"synthetic item","notes":"synthetic note"}],"vaultMirror":{"scope":"personal","plainStrings":{},"attachments":[{"itemId":"item-id","attachmentId":"attachment-id","fileName":"../recovered.txt"}]}}))
        .unwrap();
    let output_dir = dir.path().join("recovered");
    let command = |output_dir: &std::path::Path| {
        let mut command = vault_mirror();
        command
            .env("BACKUPS_DIR", dir.path())
            .env(
                "SOURCE_PASSWORD_FILE",
                dir.path().join("unavailable-source-secret"),
            )
            .env("BACKUP_PASSPHRASE", "synthetic passphrase")
            .arg("export")
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
        fs::read(output_dir.join("attachments/attachment-id/recovered.txt")).unwrap(),
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
            r#"{{"status":"success","started_at":"{finished}","finished_at":"{finished}","duration_seconds":1.0,"version":"test"}}"#
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

#[test]
fn a_misspelt_setting_in_the_settings_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("vault-mirror.env");
    for (line, named) in [
        ("INCLUDE_FOLDER=Work", "INCLUDE_FOLDER"),
        ("SCHEDULE_FILE=/run/secrets/schedule", "SCHEDULE_FILE"),
        ("pass word=hunter2", "a name"),
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
            stderr.contains(&format!("line 2 sets {named}, which is not a setting")),
            "{stderr}"
        );
        assert!(
            !stderr.contains("hunter2") && !stderr.contains("Work"),
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

/// Waits for the daemon to exit, and returns whether it succeeded and what it
/// printed.
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
