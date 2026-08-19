//! Managed child-process spawning: process-tree termination and credential
//! env scrubbing shared by `run_command` and `git` tools.
//!
//! `tokio::process::Command::kill_on_drop` only kills the direct child. For
//! shells/wrappers (e.g. `cargo` spawning `rustc`, or a shell spawning a
//! pipeline) that leaves grandchildren orphaned. [`spawn_managed`] instead
//! places the child in a platform construct that lets us (and the OS on
//! process exit) kill the whole tree:
//!
//! - Unix: the child is spawned as the leader of a new process group
//!   (`setpgid(0, 0)`); [`ManagedChild::kill_tree`] sends `SIGKILL` to the
//!   whole group via `killpg`.
//! - Windows: the child is assigned to a Job Object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; [`ManagedChild::kill_tree`] calls
//!   `TerminateJobObject`, and closing the job handle (e.g. on drop) kills
//!   the tree even if we never call `kill_tree` explicitly.

use std::io;
use std::process::Stdio;

use tokio::process::{Child, Command};

/// Env var name patterns treated as credentials and stripped from spawned
/// children's environment. Matched case-insensitively.
const EXACT_CREDENTIAL_VARS: &[&str] = &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "KODE_API_KEY"];
const CREDENTIAL_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_SECRET"];

fn is_credential_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if EXACT_CREDENTIAL_VARS.iter().any(|v| *v == upper) {
        return true;
    }
    CREDENTIAL_SUFFIXES
        .iter()
        .any(|suffix| upper.ends_with(suffix))
}

/// Removes credential-looking environment variables from `cmd` so spawned
/// children don't inherit them. Everything else is left untouched (children
/// inherit the parent's environment by default unless `env_clear` was
/// called).
pub fn scrub_env(cmd: &mut Command) {
    for (name, _) in std::env::vars() {
        if is_credential_var(&name) {
            cmd.env_remove(name);
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;

    pub struct TreeHandle {
        pid: i32,
    }

    pub fn prepare(cmd: &mut Command) {
        // Make the child the leader of a new process group so its own
        // descendants share a group id we can signal as a unit.
        unsafe {
            cmd.pre_exec(|| {
                let rc = libc::setpgid(0, 0);
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    pub fn attach(child: &Child) -> io::Result<TreeHandle> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no pid (already reaped)"))?;
        Ok(TreeHandle { pid: pid as i32 })
    }

    impl TreeHandle {
        pub fn kill_tree(&mut self) {
            // Negative pid targets the whole process group.
            unsafe {
                libc::killpg(self.pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub struct TreeHandle {
        job: HANDLE,
    }

    // Safety: HANDLE is just a raw pointer-sized value; we only ever use it
    // from the owning task and close it exactly once.
    unsafe impl Send for TreeHandle {}

    pub fn prepare(_cmd: &mut Command) {
        // Nothing to do pre-spawn on Windows; the job object is created and
        // the process assigned to it after spawn, in `attach`.
    }

    pub fn attach(child: &Child) -> io::Result<TreeHandle> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                let err = io::Error::last_os_error();
                CloseHandle(job);
                return Err(err);
            }

            let Some(raw_handle) = child.raw_handle() else {
                CloseHandle(job);
                return Err(io::Error::other("child has no handle (already reaped)"));
            };
            let process_handle = raw_handle as HANDLE;
            let ok = AssignProcessToJobObject(job, process_handle);
            if ok == 0 {
                let err = io::Error::last_os_error();
                CloseHandle(job);
                return Err(err);
            }

            Ok(TreeHandle { job })
        }
    }

    impl TreeHandle {
        pub fn kill_tree(&mut self) {
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }
    }

    impl Drop for TreeHandle {
        fn drop(&mut self) {
            // Closing the job handle kills every process still assigned to
            // it, because the job was created with KILL_ON_JOB_CLOSE.
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}

/// Owns whatever platform state is needed to kill an entire process tree
/// (not just the direct child). Kept separate from [`Child`] so callers that
/// need to `.await` the child's output inside a `tokio::select!` branch (or
/// `tokio::time::timeout`) can split a [`ManagedChild`] via
/// [`ManagedChild::into_parts`] and still retain the ability to kill the
/// tree from a *different* branch — the `Child` gets moved into the wait
/// future, `TreeGuard` stays a free-standing local unaffected by that move.
///
/// Dropping a `TreeGuard` that was never explicitly killed still kills the
/// tree (best-effort), so it's safe to just let it fall out of scope after
/// the awaited command completes normally.
pub struct TreeGuard {
    tree: Option<platform::TreeHandle>,
}

impl TreeGuard {
    /// Kills every process in the tree. No-op (silently) if attaching the
    /// tree handle failed at spawn time — see [`spawn_managed`].
    pub fn kill_tree(&mut self) {
        if let Some(tree) = self.tree.as_mut() {
            tree.kill_tree();
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        self.kill_tree();
    }
}

/// A spawned child process plus its [`TreeGuard`]. Intentionally does not
/// implement `Drop` itself (only `TreeGuard` does) so it can be freely
/// destructured — see [`ManagedChild::into_parts`].
pub struct ManagedChild {
    child: Child,
    tree: TreeGuard,
}

impl ManagedChild {
    /// The OS-assigned process id of the direct child, if it hasn't already
    /// been reaped. See [`Child::id`].
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Kills the child and (best-effort) every process it spawned.
    pub fn kill_tree(&mut self) {
        if self.tree.tree.is_some() {
            self.tree.kill_tree();
        } else {
            // No tree handle (e.g. attach failed) — fall back to killing
            // just the direct child.
            let _ = self.child.start_kill();
        }
    }

    /// Waits for the child to exit and collects its output, consuming self.
    /// Use [`Self::into_parts`] instead when the wait needs to run inside a
    /// `tokio::select!`/`timeout` alongside the ability to call
    /// [`TreeGuard::kill_tree`] from a sibling branch.
    pub async fn wait_with_output(self) -> io::Result<std::process::Output> {
        self.child.wait_with_output().await
    }

    /// Splits into the raw [`Child`] (to `.await` its output) and a
    /// [`TreeGuard`] (to kill the tree from elsewhere, e.g. a timeout or
    /// cancellation branch racing the wait).
    pub fn into_parts(self) -> (Child, TreeGuard) {
        (self.child, self.tree)
    }
}

/// Spawns `cmd`, arranging for [`ManagedChild::kill_tree`] / [`TreeGuard`]
/// (including its `Drop`) to terminate the whole process tree rather than
/// just the direct child.
///
/// Also sets `kill_on_drop(true)` as a belt-and-braces fallback for the
/// direct child in case tree-tracking setup fails.
pub fn spawn_managed(cmd: &mut Command) -> io::Result<ManagedChild> {
    platform::prepare(cmd);
    cmd.kill_on_drop(true);
    let child = cmd.spawn()?;
    let tree = match platform::attach(&child) {
        Ok(tree) => Some(tree),
        Err(e) => {
            tracing::warn!(error = %e, "failed to attach process-tree handle; falling back to direct-child kill only");
            None
        }
    };
    Ok(ManagedChild {
        child,
        tree: TreeGuard { tree },
    })
}

/// Convenience: builds a `Command` with the standard stdio wiring used by
/// tool subprocesses (null stdin, piped stdout/stderr) and scrubbed env.
pub fn managed_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut cmd);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;

    #[test]
    fn scrub_detects_exact_names_case_insensitive() {
        assert!(is_credential_var("ANTHROPIC_API_KEY"));
        assert!(is_credential_var("anthropic_api_key"));
        assert!(is_credential_var("OPENAI_API_KEY"));
        assert!(is_credential_var("KODE_API_KEY"));
    }

    #[test]
    fn scrub_detects_suffixes_case_insensitive() {
        assert!(is_credential_var("MY_CUSTOM_API_KEY"));
        assert!(is_credential_var("my_custom_api_key"));
        assert!(is_credential_var("GITHUB_TOKEN"));
        assert!(is_credential_var("github_token"));
        assert!(is_credential_var("DB_SECRET"));
        assert!(is_credential_var("db_secret"));
    }

    #[test]
    fn scrub_leaves_non_matching_vars_untouched() {
        assert!(!is_credential_var("PATH"));
        assert!(!is_credential_var("HOME"));
        assert!(!is_credential_var("RUST_LOG"));
        assert!(!is_credential_var("TOKENIZER")); // contains "TOKEN" but no underscore-prefixed suffix match
    }

    #[test]
    fn scrub_env_removes_matching_vars_from_command() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: guarded by ENV_LOCK, single-threaded within this test.
        unsafe {
            std::env::set_var("KODE_TEST_SECRET_TOKEN", "shh");
            std::env::set_var("KODE_TEST_HARMLESS_VAR", "ok");
        }

        let mut cmd = Command::new("does-not-matter");
        scrub_env(&mut cmd);

        // `env_remove` records an explicit override (key -> None) in the
        // std::process::Command env map, which get_envs() surfaces.
        let envs: std::collections::HashMap<_, _> = cmd.as_std().get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("KODE_TEST_SECRET_TOKEN")),
            Some(&None),
            "credential var should be explicitly removed"
        );
        assert!(
            !envs.contains_key(std::ffi::OsStr::new("KODE_TEST_HARMLESS_VAR")),
            "non-credential var should be left inherited, not touched"
        );

        unsafe {
            std::env::remove_var("KODE_TEST_SECRET_TOKEN");
            std::env::remove_var("KODE_TEST_HARMLESS_VAR");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_managed_runs_trivial_command() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "echo hi"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let managed = spawn_managed(&mut cmd).unwrap();
        let output = managed.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_managed_runs_trivial_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hi"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let managed = spawn_managed(&mut cmd).unwrap();
        let output = managed.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hi"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn kill_tree_kills_grandchild() {
        // Parent: cmd.exe spawns a detached long-running child (ping) via
        // `start`. kill_tree should take down both.
        let mut cmd = Command::new("cmd");
        cmd.args([
            "/C",
            "start /B ping -n 30 127.0.0.1 >nul & ping -n 30 127.0.0.1 >nul",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut managed = spawn_managed(&mut cmd).unwrap();
        let parent_pid = managed.id().expect("child should have a pid");

        // Let the tree actually start.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        managed.kill_tree();

        // Give the OS a moment to tear the tree down, then verify no
        // surviving ping.exe processes whose ParentProcessId is *this
        // test's* cmd.exe. Scoping by parent pid (rather than a blanket
        // image-name search) keeps this test independent of any other
        // concurrently running test that also happens to spawn ping.exe.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let check = tokio::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "(Get-CimInstance Win32_Process -Filter \"Name='ping.exe' AND ParentProcessId={parent_pid}\").ProcessId"
                ),
            ])
            .output()
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&check.stdout);
        assert!(
            out.trim().is_empty(),
            "expected no surviving ping.exe children of pid {parent_pid}, got pids: {out}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_tree_kills_grandchild() {
        // Parent: sh spawns a long-running grandchild via a subshell so it
        // has its own pid but inherits the parent's process group (set to
        // the parent's own pid by `platform::prepare`'s `setpgid(0, 0)`).
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut managed = spawn_managed(&mut cmd).unwrap();
        let pgid = managed.id().expect("child should have a pid");

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        managed.kill_tree();

        // Reap the direct child: after SIGKILL it lingers as a zombie until
        // waited on, and `pgrep -g` reports zombies as live group members.
        let _ = managed.wait_with_output().await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Query by process *group* id (not a name/command pattern), so this
        // test is independent of any other concurrently running test that
        // also happens to spawn a `sleep` process.
        let check = tokio::process::Command::new("pgrep")
            .args(["-g", &pgid.to_string()])
            .output()
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&check.stdout);
        assert!(
            out.trim().is_empty(),
            "expected no surviving processes in group {pgid}, got pids: {out}"
        );
    }
}
