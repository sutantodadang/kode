//! Runs the embedded `train_laya.py` locally through `uv`, or on HF Jobs
//! through the `hf` CLI. The script does training + ONNX export only.

use std::path::PathBuf;

use anyhow::Context;

pub const TRAIN_SCRIPT: &str = include_str!("../../../scripts/local-models/train_laya.py");

pub struct TrainJob {
    pub id: String,
    pub script: PathBuf,
    pub train_jsonl: PathBuf,
    /// Where `model.onnx`, `laya.json`, `tokenizer.json` must end up.
    pub out_dir: PathBuf,
}

#[async_trait::async_trait]
pub trait TrainRunner: Send + Sync {
    async fn run(&self, job: &TrainJob) -> anyhow::Result<()>;
}

pub fn uv_args(job: &TrainJob) -> Vec<String> {
    vec![
        "run".to_string(),
        job.script.to_string_lossy().to_string(),
        "--train".to_string(),
        job.train_jsonl.to_string_lossy().to_string(),
        "--out".to_string(),
        job.out_dir.to_string_lossy().to_string(),
    ]
}

pub fn hf_job_args(job: &TrainJob, repo: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "jobs",
        "uv",
        "run",
        "--flavor",
        "t4-small",
        "--secrets",
        "HF_TOKEN",
        "--timeout",
        "2h",
        "--detach",
        "-q",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(job.script.to_string_lossy().to_string());
    a.extend([
        "--hub-dataset".to_string(),
        repo.to_string(),
        "--hub-train".to_string(),
        format!("train/{}.jsonl", job.id),
        "--push-branch".to_string(),
        format!("candidate-{}", job.id),
    ]);
    a
}

fn hf_command(args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("hf");
    cmd.args(args);
    if std::env::var_os("HF_TOKEN").is_none()
        && let Some(token) = crate::setup::hf_token()
    {
        cmd.env("HF_TOKEN", token);
    }
    cmd
}

/// Runs `hf …` with inherited output; fails with the command on error.
pub async fn hf(args: &[&str]) -> anyhow::Result<()> {
    let status = hf_command(args)
        .status()
        .await
        .context("`hf` not found — install the Hugging Face CLI")?;
    if !status.success() {
        anyhow::bail!("`hf {}` failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Runs `hf …` and returns its stdout.
pub async fn hf_output(args: &[&str]) -> anyhow::Result<String> {
    let out = hf_command(args)
        .output()
        .await
        .context("`hf` not found — install the Hugging Face CLI")?;
    if !out.status.success() {
        anyhow::bail!(
            "`hf {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub struct UvRunner;

#[async_trait::async_trait]
impl TrainRunner for UvRunner {
    async fn run(&self, job: &TrainJob) -> anyhow::Result<()> {
        let status = tokio::process::Command::new("uv")
            .args(uv_args(job))
            .status()
            .await
            .context("`uv` not found — install it: https://docs.astral.sh/uv/")?;
        if !status.success() {
            anyhow::bail!("training failed (uv exited with {status})");
        }
        Ok(())
    }
}

pub struct HfJobsRunner {
    pub dataset_repo: String,
}

#[async_trait::async_trait]
impl TrainRunner for HfJobsRunner {
    async fn run(&self, job: &TrainJob) -> anyhow::Result<()> {
        let repo = self.dataset_repo.as_str();
        let train_in_repo = format!("train/{}.jsonl", job.id);
        let branch = format!("candidate-{}", job.id);
        hf(&[
            "repos",
            "create",
            repo,
            "--type",
            "dataset",
            "--private",
            "--exist-ok",
        ])
        .await?;
        hf(&[
            "upload",
            repo,
            &job.train_jsonl.to_string_lossy(),
            &train_in_repo,
            "--type",
            "dataset",
            "--commit-message",
            &format!("kode router train {}", job.id),
        ])
        .await?;
        let args = hf_job_args(job, repo);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let job_id = hf_output(&refs)
            .await?
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .context("hf jobs did not print a job id")?
            .to_string();
        println!("HF job {job_id} started — follow with: hf jobs logs {job_id} --follow");
        hf(&["jobs", "wait", &job_id]).await?;
        let download = job.out_dir.with_extension("download");
        hf(&[
            "download",
            repo,
            "--type",
            "dataset",
            "--revision",
            &branch,
            "--include",
            "candidate/*",
            "--local-dir",
            &download.to_string_lossy(),
        ])
        .await
        .with_context(|| format!("no outputs on {repo}@{branch} — check: hf jobs logs {job_id}"))?;
        std::fs::rename(download.join("candidate"), &job.out_dir)
            .with_context(|| format!("moving outputs into {}", job.out_dir.display()))?;
        let _ = std::fs::remove_dir_all(&download);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> TrainJob {
        TrainJob {
            id: "01JABC".to_string(),
            script: PathBuf::from("c/train_laya.py"),
            train_jsonl: PathBuf::from("c/train.jsonl"),
            out_dir: PathBuf::from("c/model"),
        }
    }

    #[test]
    fn embedded_script_is_the_training_script() {
        assert!(TRAIN_SCRIPT.starts_with("# /// script"));
        assert!(TRAIN_SCRIPT.contains("proper_reward"));
        assert!(TRAIN_SCRIPT.contains("--push-branch"));
    }

    #[test]
    fn uv_args_run_the_script_on_local_files() {
        let a = uv_args(&job());
        assert_eq!(a[0], "run");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--train" && w[1].ends_with("train.jsonl"))
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--out" && w[1].ends_with("model"))
        );
    }

    #[test]
    fn hf_job_args_put_options_before_the_script() {
        let a = hf_job_args(&job(), "team/data");
        let script_at = a.iter().position(|x| x.ends_with("train_laya.py")).unwrap();
        let flavor_at = a.iter().position(|x| x == "--flavor").unwrap();
        assert!(flavor_at < script_at);
        assert!(a.contains(&"t4-small".to_string()));
        assert!(a.contains(&"--detach".to_string()));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--push-branch" && w[1] == "candidate-01JABC")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--hub-train" && w[1] == "train/01JABC.jsonl")
        );
    }
}
