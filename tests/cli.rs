//! The command line itself, run as a separate process. None of these tests
//! need the sandbox servers.

use std::process::Command;

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
