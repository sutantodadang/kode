//! Fresh native install, including a home on a different volume from TEMP.
//! KODE_SETUP_QA_HOME=G:/kode-work cargo test -p kode --test setup_real -- --ignored

use std::path::Path;
use std::process::Command;

fn run(repo: &Path, profile: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_kode"))
        .args(args)
        .current_dir(repo)
        .env("USERPROFILE", profile)
        .env_remove("KODE_ZINDEKS_DYLIB")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{args:?}: {text}");
    text
}

#[test]
#[ignore = "downloads the real zindeks asset; requires KODE_SETUP_QA_HOME"]
fn fresh_native_setup_then_index_and_remember() {
    let base = std::env::var_os("KODE_SETUP_QA_HOME").expect("set KODE_SETUP_QA_HOME");
    let root = std::path::PathBuf::from(base).join(format!(
        "kode-setup-real-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let repo = root.join("repo");
    let profile = root.join("profile");
    std::fs::create_dir_all(repo.join(".kode")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join(".kode/config.toml"), "[router]\nenabled=false\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();

    run(&repo, &profile, &["setup", "--yes"]);
    assert!(run(&repo, &profile, &["index"]).contains("1 symbols"));
    assert!(
        run(
            &repo,
            &profile,
            &["remember", "fresh native installation works"]
        )
        .contains("remembered")
    );
    assert!(run(&repo, &profile, &["setup", "--yes"]).contains("library found"));
    // Simulate an installation containing only the previous release. Setup
    // must upgrade rather than accepting any existing DLL or sorting hashes.
    let runtime = profile.join(".kode/runtime/zindeks");
    let current = runtime.join("418da3ff065e956509b0e6746e9c17075d920c19");
    let old = runtime.join("e79bc89a3b68870448db5c69d4420ec509484959");
    std::fs::create_dir_all(&old).unwrap();
    let library = if cfg!(windows) {
        "zindeks.dll"
    } else if cfg!(target_os = "macos") {
        "libzindeks.dylib"
    } else {
        "libzindeks.so"
    };
    std::fs::write(old.join(library), "old fixture; must not load").unwrap();
    std::fs::write(old.join("metadata.json"), r#"{"abi_version":1,"zindeks_version":"0.10.2","source_revision":"e79bc89a3b68870448db5c69d4420ec509484959"}"#).unwrap();
    assert!(
        current
            .canonicalize()
            .unwrap()
            .starts_with(root.canonicalize().unwrap())
    );
    std::fs::rename(&current, root.join("previous-pinned-install")).unwrap();
    let upgraded = run(&repo, &profile, &["setup", "--yes"]);
    assert!(upgraded.contains("library installed"), "{upgraded}");
    assert!(current.join(library).is_file());
    assert!(run(&repo, &profile, &["index"]).contains("1 symbols"));
    assert!(run(&repo, &profile, &["setup", "--yes"]).contains("library found"));
    assert!(profile.join(".kode/ingat/memory.sqlite3").is_file());
    std::fs::remove_dir_all(root).unwrap();
}
