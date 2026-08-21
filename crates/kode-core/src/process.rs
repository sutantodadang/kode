//! Cross-platform managed subprocess support.
//!
//! Children are placed in a Unix process group or Windows Job Object so
//! timeout/cancellation can terminate descendants as well as the direct
//! child. Credential-looking environment variables are removed before
//! spawning commands through [`managed_command`] or [`scrub_env`].

use std::io;
use std::process::Stdio;
use tokio::process::{Child, Command};

const EXACT_CREDENTIAL_VARS: &[&str] = &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "KODE_API_KEY"];
const CREDENTIAL_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_SECRET"];

fn is_credential_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    EXACT_CREDENTIAL_VARS.iter().any(|v| *v == upper)
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| upper.ends_with(suffix))
}

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
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    pub fn attach(child: &Child) -> io::Result<TreeHandle> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no pid"))?;
        Ok(TreeHandle { pid: pid as i32 })
    }

    impl TreeHandle {
        pub fn kill_tree(&mut self) {
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
    unsafe impl Send for TreeHandle {}

    pub fn prepare(_cmd: &mut Command) {}

    pub fn attach(child: &Child) -> io::Result<TreeHandle> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(error);
            }
            let Some(raw) = child.raw_handle() else {
                CloseHandle(job);
                return Err(io::Error::other("child has no handle"));
            };
            if AssignProcessToJobObject(job, raw as HANDLE) == 0 {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(error);
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
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}

pub struct TreeGuard {
    tree: Option<platform::TreeHandle>,
}

impl TreeGuard {
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

pub struct ManagedChild {
    child: Child,
    tree: TreeGuard,
}

impl ManagedChild {
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub fn kill_tree(&mut self) {
        if self.tree.tree.is_some() {
            self.tree.kill_tree();
        } else {
            let _ = self.child.start_kill();
        }
    }
    pub async fn wait_with_output(self) -> io::Result<std::process::Output> {
        self.child.wait_with_output().await
    }
    pub fn into_parts(self) -> (Child, TreeGuard) {
        (self.child, self.tree)
    }
}

pub fn spawn_managed(cmd: &mut Command) -> io::Result<ManagedChild> {
    platform::prepare(cmd);
    cmd.kill_on_drop(true);
    let child = cmd.spawn()?;
    let tree = match platform::attach(&child) {
        Ok(tree) => Some(tree),
        Err(error) => {
            tracing::warn!(%error, "failed to attach process-tree handle; using direct-child fallback");
            None
        }
    };
    Ok(ManagedChild {
        child,
        tree: TreeGuard { tree },
    })
}

pub fn managed_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_credentials_without_false_substring_matches() {
        assert!(is_credential_var("github_token"));
        assert!(is_credential_var("custom_api_key"));
        assert!(!is_credential_var("TOKENIZER"));
        assert!(!is_credential_var("PATH"));
    }

    #[tokio::test]
    async fn managed_child_runs() {
        let mut command = if cfg!(windows) {
            let mut c = managed_command("cmd");
            c.args(["/C", "echo hi"]);
            c
        } else {
            let mut c = managed_command("sh");
            c.args(["-c", "printf hi"]);
            c
        };
        let output = spawn_managed(&mut command)
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hi"));
    }
}
