use std::process::Command;

#[test]
fn status_command_prints_expected_fields() {
    let output = Command::new(env!("CARGO_BIN_EXE_kode"))
        .arg("status")
        .output()
        .expect("failed to run kode binary");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Kode v"), "stdout was: {stdout}");
    assert!(stdout.contains("zindeks"), "stdout was: {stdout}");
}
