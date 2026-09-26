//! ONNX Runtime bootstrap (`load-dynamic`) and execution-provider choice.
//! A GPU EP that fails to register falls back to CPU, and the fallback is
//! reported in [`Device`], never hidden.

use std::path::Path;
use std::sync::OnceLock;

use ort::execution_providers::{
    CUDAExecutionProvider, CoreMLExecutionProvider, DirectMLExecutionProvider,
    ExecutionProviderDispatch,
};
use ort::session::Session;

use crate::error::{LocalError, rt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePref {
    Auto,
    Cpu,
    DirectMl,
    Cuda,
    CoreMl,
}

impl DevicePref {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "cpu" => Some(Self::Cpu),
            "directml" => Some(Self::DirectMl),
            "cuda" => Some(Self::Cuda),
            "coreml" => Some(Self::CoreMl),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ep {
    DirectMl,
    Cuda,
    CoreMl,
}

impl Ep {
    pub fn name(self) -> &'static str {
        match self {
            Ep::DirectMl => "directml",
            Ep::Cuda => "cuda",
            Ep::CoreMl => "coreml",
        }
    }

    fn dispatch(self) -> ExecutionProviderDispatch {
        match self {
            Ep::DirectMl => DirectMLExecutionProvider::default().build(),
            Ep::Cuda => CUDAExecutionProvider::default().build(),
            Ep::CoreMl => CoreMLExecutionProvider::default().build(),
        }
        .error_on_failure()
    }
}

/// GPU EPs to try, in order, before CPU.
pub fn ep_plan(pref: DevicePref, os: &str) -> Vec<Ep> {
    match pref {
        DevicePref::Cpu => vec![],
        DevicePref::DirectMl => vec![Ep::DirectMl],
        DevicePref::Cuda => vec![Ep::Cuda],
        DevicePref::CoreMl => vec![Ep::CoreMl],
        DevicePref::Auto => match os {
            "windows" => vec![Ep::DirectMl],
            "macos" => vec![Ep::CoreMl],
            "linux" => vec![Ep::Cuda],
            _ => vec![],
        },
    }
}

pub fn describe_plan(pref: DevicePref, os: &str) -> String {
    let plan = ep_plan(pref, os);
    if plan.is_empty() {
        return "cpu".to_string();
    }
    let names: Vec<&str> = plan.iter().map(|e| e.name()).collect();
    let head = if pref == DevicePref::Auto {
        "auto → "
    } else {
        ""
    };
    format!("{head}{}, cpu fallback", names.join(", "))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Device {
    Gpu(&'static str),
    Cpu { gpu_error: Option<String> },
}

impl Device {
    pub fn is_gpu(&self) -> bool {
        matches!(self, Device::Gpu(_))
    }

    pub fn label(&self) -> String {
        match self {
            Device::Gpu(name) => (*name).to_string(),
            Device::Cpu { gpu_error: None } => "cpu".to_string(),
            Device::Cpu { gpu_error: Some(e) } => format!("cpu (gpu unavailable: {e})"),
        }
    }
}

static ORT: OnceLock<Result<(), String>> = OnceLock::new();

/// Loads the ONNX Runtime shared library once per process. Later calls
/// return the first call's result (the dylib cannot be swapped in-process).
pub fn init_runtime(dylib: &Path) -> Result<(), LocalError> {
    ORT.get_or_init(|| {
        ort::init_from(dylib.to_string_lossy().to_string())
            .commit()
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .clone()
    .map_err(LocalError::Runtime)
}

fn session_with(model: &Path, eps: Vec<ExecutionProviderDispatch>) -> Result<Session, LocalError> {
    Session::builder()
        .map_err(rt)?
        .with_execution_providers(eps)
        .map_err(rt)?
        .commit_from_file(model)
        .map_err(rt)
}

/// Builds a session on the first GPU EP that registers, else on CPU.
/// `init_runtime` must have succeeded first.
pub fn build_session(model: &Path, pref: DevicePref) -> Result<(Session, Device), LocalError> {
    let mut gpu_error = None;
    for ep in ep_plan(pref, std::env::consts::OS) {
        match session_with(model, vec![ep.dispatch()]) {
            Ok(session) => return Ok((session, Device::Gpu(ep.name()))),
            Err(e) => gpu_error = Some(e.to_string()),
        }
    }
    let session = session_with(model, vec![])?;
    Ok((session, Device::Cpu { gpu_error }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_documented_values_only() {
        assert_eq!(DevicePref::parse("auto"), Some(DevicePref::Auto));
        assert_eq!(DevicePref::parse("CPU"), Some(DevicePref::Cpu));
        assert_eq!(DevicePref::parse("directml"), Some(DevicePref::DirectMl));
        assert_eq!(DevicePref::parse("cuda"), Some(DevicePref::Cuda));
        assert_eq!(DevicePref::parse("coreml"), Some(DevicePref::CoreMl));
        assert_eq!(DevicePref::parse("vulkan"), None);
    }

    #[test]
    fn auto_plan_follows_the_platform() {
        assert_eq!(ep_plan(DevicePref::Auto, "windows"), vec![Ep::DirectMl]);
        assert_eq!(ep_plan(DevicePref::Auto, "macos"), vec![Ep::CoreMl]);
        assert_eq!(ep_plan(DevicePref::Auto, "linux"), vec![Ep::Cuda]);
        assert!(ep_plan(DevicePref::Auto, "freebsd").is_empty());
        assert!(ep_plan(DevicePref::Cpu, "windows").is_empty());
        assert_eq!(ep_plan(DevicePref::Cuda, "windows"), vec![Ep::Cuda]);
    }

    #[test]
    fn describe_plan_names_the_fallback() {
        assert_eq!(
            describe_plan(DevicePref::Auto, "windows"),
            "auto → directml, cpu fallback"
        );
        assert_eq!(describe_plan(DevicePref::Cpu, "linux"), "cpu");
    }

    #[test]
    fn device_labels() {
        assert_eq!(Device::Gpu("directml").label(), "directml");
        assert_eq!(Device::Cpu { gpu_error: None }.label(), "cpu");
        assert_eq!(
            Device::Cpu {
                gpu_error: Some("no adapter".to_string())
            }
            .label(),
            "cpu (gpu unavailable: no adapter)"
        );
        assert!(Device::Gpu("cuda").is_gpu());
        assert!(!Device::Cpu { gpu_error: None }.is_gpu());
    }

    /// Needs a real ONNX Runtime: `KODE_ORT_DYLIB=/path/to/onnxruntime.dll`.
    #[test]
    #[ignore]
    fn runtime_initialises_from_env_dylib() {
        let dylib = std::env::var_os("KODE_ORT_DYLIB").expect("set KODE_ORT_DYLIB");
        init_runtime(Path::new(&dylib)).unwrap();
        init_runtime(Path::new(&dylib)).unwrap();
    }
}
