//! GPU/accelerator detection for automatic PyTorch index selection.
//!
//! When `torch-backend = "auto"` is set, vyp queries the system for
//! NVIDIA CUDA, AMD ROCm, or Intel XPU hardware and maps the result
//! to the correct PyTorch wheel index URL.

use std::process::Command;

/// Packages that should be routed to the PyTorch index when a torch
/// backend is active.
pub const TORCH_PACKAGES: &[&str] = &[
    "torch",
    "torchvision",
    "torchaudio",
    "pytorch-triton",
    "pytorch-triton-rocm",
    "pytorch-triton-xpu",
    "torch-model-archiver",
    "torch-tb-profiler",
];

/// Detected or configured accelerator backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceleratorBackend {
    Cpu,
    Cuda(String),
    Rocm(String),
    Xpu,
}

impl AcceleratorBackend {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let s = s.to_lowercase();
        if s == "cpu" {
            Self::Cpu
        } else if s.starts_with("cu") {
            Self::Cuda(s)
        } else if s.starts_with("rocm") {
            Self::Rocm(s)
        } else if s == "xpu" {
            Self::Xpu
        } else {
            // Unknown value — treat as CPU
            tracing::warn!("Unknown torch-backend '{}', falling back to cpu", s);
            Self::Cpu
        }
    }
}

impl std::fmt::Display for AcceleratorBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => write!(f, "cpu"),
            Self::Cuda(tag) => write!(f, "{}", tag),
            Self::Rocm(tag) => write!(f, "{}", tag),
            Self::Xpu => write!(f, "xpu"),
        }
    }
}

/// Detect the best accelerator backend available on this system.
pub fn detect_backend() -> AcceleratorBackend {
    if let Some(cuda) = detect_cuda() {
        return cuda;
    }
    if let Some(rocm) = detect_rocm() {
        return rocm;
    }
    if detect_xpu() {
        return AcceleratorBackend::Xpu;
    }
    tracing::info!("No GPU detected, using CPU-only PyTorch backend");
    AcceleratorBackend::Cpu
}

/// Map a backend to its PyTorch wheel index URL.
/// Returns `None` for CPU (PyPI already has CPU-only wheels).
pub fn backend_index_url(backend: &AcceleratorBackend) -> Option<&'static str> {
    match backend {
        AcceleratorBackend::Cpu => Some("https://download.pytorch.org/whl/cpu"),
        AcceleratorBackend::Cuda(tag) => match tag.as_str() {
            "cu118" => Some("https://download.pytorch.org/whl/cu118"),
            "cu121" => Some("https://download.pytorch.org/whl/cu121"),
            "cu124" => Some("https://download.pytorch.org/whl/cu124"),
            "cu126" => Some("https://download.pytorch.org/whl/cu126"),
            "cu128" => Some("https://download.pytorch.org/whl/cu128"),
            "cu130" => Some("https://download.pytorch.org/whl/cu130"),
            _ => {
                tracing::warn!("Unknown CUDA tag '{}', falling back to cu128", tag);
                Some("https://download.pytorch.org/whl/cu128")
            }
        },
        AcceleratorBackend::Rocm(tag) => match tag.as_str() {
            "rocm6" | "rocm6.4" => Some("https://download.pytorch.org/whl/rocm6.4"),
            _ => Some("https://download.pytorch.org/whl/rocm6.4"),
        },
        AcceleratorBackend::Xpu => Some("https://download.pytorch.org/whl/xpu"),
    }
}

// ---------------------------------------------------------------------------
// Detection helpers
// ---------------------------------------------------------------------------

fn detect_cuda() -> Option<AcceleratorBackend> {
    // Try nvidia-smi first (works on Linux, Windows, WSL)
    if let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Take first line only (multi-GPU systems print one line per GPU)
            if let Some(line) = stdout.lines().next() {
                if let Some(backend) = driver_version_to_cuda_tag(line.trim()) {
                    tracing::info!("Detected NVIDIA driver {}, using {}", line.trim(), backend);
                    return Some(backend);
                }
            }
        }
    }

    // Fallback: /proc/driver/nvidia/version on Linux
    if let Ok(content) = std::fs::read_to_string("/proc/driver/nvidia/version") {
        // Format: "NVRM version: NVIDIA UNIX x86_64 Kernel Module  535.129.03 ..."
        for word in content.split_whitespace() {
            if word.contains('.') && word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                if let Some(backend) = driver_version_to_cuda_tag(word) {
                    return Some(backend);
                }
            }
        }
    }

    None
}

fn driver_version_to_cuda_tag(version_str: &str) -> Option<AcceleratorBackend> {
    let major: u32 = version_str.split('.').next()?.parse().ok()?;

    // NVIDIA driver -> CUDA toolkit mapping (approximate)
    let tag = if major >= 570 {
        "cu130"
    } else if major >= 560 {
        "cu128"
    } else if major >= 550 {
        "cu126"
    } else if major >= 545 {
        "cu124"
    } else if major >= 525 {
        "cu121"
    } else if major >= 450 {
        "cu118"
    } else {
        return None;
    };

    Some(AcceleratorBackend::Cuda(tag.to_string()))
}

fn detect_rocm() -> Option<AcceleratorBackend> {
    // Check for rocm-smi
    if let Ok(output) = Command::new("rocm-smi").arg("--showdriverversion").output() {
        if output.status.success() {
            tracing::info!("Detected AMD ROCm GPU");
            return Some(AcceleratorBackend::Rocm("rocm6.4".to_string()));
        }
    }

    // Check for rocm_agent_enumerator
    if std::path::Path::new("/opt/rocm/bin/rocm_agent_enumerator").exists() {
        tracing::info!("Detected AMD ROCm installation at /opt/rocm");
        return Some(AcceleratorBackend::Rocm("rocm6.4".to_string()));
    }

    None
}

fn detect_xpu() -> bool {
    // Check for Intel xpu-smi
    if let Ok(output) = Command::new("xpu-smi").arg("discovery").output() {
        if output.status.success() {
            tracing::info!("Detected Intel XPU");
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_from_str() {
        assert_eq!(AcceleratorBackend::from_str("cpu"), AcceleratorBackend::Cpu);
        assert_eq!(
            AcceleratorBackend::from_str("cu128"),
            AcceleratorBackend::Cuda("cu128".into())
        );
        assert_eq!(
            AcceleratorBackend::from_str("rocm6"),
            AcceleratorBackend::Rocm("rocm6".into())
        );
        assert_eq!(AcceleratorBackend::from_str("xpu"), AcceleratorBackend::Xpu);
    }

    #[test]
    fn test_driver_version_to_cuda_tag() {
        assert_eq!(
            driver_version_to_cuda_tag("535.129.03"),
            Some(AcceleratorBackend::Cuda("cu121".into()))
        );
        assert_eq!(
            driver_version_to_cuda_tag("560.35.03"),
            Some(AcceleratorBackend::Cuda("cu128".into()))
        );
        assert_eq!(driver_version_to_cuda_tag("400.0"), None);
    }

    #[test]
    fn test_backend_index_url() {
        assert_eq!(
            backend_index_url(&AcceleratorBackend::Cpu),
            Some("https://download.pytorch.org/whl/cpu")
        );
        assert_eq!(
            backend_index_url(&AcceleratorBackend::Cuda("cu128".into())),
            Some("https://download.pytorch.org/whl/cu128")
        );
    }

    #[test]
    fn test_torch_packages_list() {
        assert!(TORCH_PACKAGES.contains(&"torch"));
        assert!(TORCH_PACKAGES.contains(&"torchvision"));
        assert!(TORCH_PACKAGES.contains(&"torchaudio"));
    }
}
