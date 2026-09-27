use std::process::Command;

#[test]
fn status_command_prints_expected_fields() {
    // Run in an isolated temp home so the native memory store is created
    // there, never in the real `~/.kode`.
    let home = std::env::temp_dir().join(format!("kode-status-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kode"))
        .arg("status")
        .current_dir(&home)
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .output()
        .expect("failed to run kode binary");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Kode v"), "stdout was: {stdout}");
    assert!(stdout.contains("zindeks"), "stdout was: {stdout}");

    let _ = std::fs::remove_dir_all(&home);
}
