use std::process::Command;

#[test]
fn cli_help_smoke() {
    let output = Command::new(env!("CARGO_BIN_EXE_codex-telegram-bridge"))
        .arg("--help")
        .output()
        .expect("run binary");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("telegram"));
    assert!(stdout.contains("projects"));
}
