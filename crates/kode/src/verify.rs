use std::path::Path;

use kode_core::CancellationToken;

/// Standalone `kode verify`: detect the project kind at `cwd`, run its
/// verification pipeline, and print the report. Exits with an error when
/// verification did not pass.
pub async fn run(cwd: &Path, cancel: CancellationToken) -> anyhow::Result<()> {
    let profile = kode_verify::detect(cwd);
    println!(
        "verifying ({:?}): {} steps",
        profile.kind,
        profile.steps.len()
    );

    let report = kode_verify::run_verification(cwd, &profile, &cancel).await;
    println!("{}", report.render());
    println!("{}", report.summary_line());

    if report.ok {
        Ok(())
    } else {
        anyhow::bail!("verification failed")
    }
}
