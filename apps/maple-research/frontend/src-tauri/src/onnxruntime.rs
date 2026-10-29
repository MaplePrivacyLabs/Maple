use once_cell::sync::Lazy;
#[cfg(not(target_os = "ios"))]
use std::path::PathBuf;

static ORT_ENVIRONMENT: Lazy<Result<(), String>> = Lazy::new(initialize);

/// Initializes Maple's ONNX Runtime environment before OCR creates a session.
/// Dynamic targets use the explicitly packaged runtime; iOS keeps its existing
/// static-library linkage.
pub fn ensure_initialized() -> Result<(), String> {
    ORT_ENVIRONMENT
        .as_ref()
        .map(|_| ())
        .map_err(std::clone::Clone::clone)
}

fn initialize() -> Result<(), String> {
    #[cfg(not(target_os = "ios"))]
    {
        let path = runtime_library_path();
        let builder = ort::init_from(&path).map_err(|error| {
            format!(
                "Failed to load ONNX Runtime from {}: {error}",
                path.display()
            )
        })?;
        let _ = builder.commit();
    }

    #[cfg(target_os = "ios")]
    {
        let _ = ort::init().commit();
    }

    Ok(())
}

#[cfg(not(target_os = "ios"))]
fn runtime_library_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH").filter(|path| !path.is_empty()) {
        return path.into();
    }

    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            #[cfg(target_os = "linux")]
            {
                let library_directory = if option_env!("MAPLE_DESKTOP_VARIANT") == Some("dev") {
                    "maple-research-dev"
                } else {
                    "maple"
                };
                let bundled = directory
                    .join("../lib")
                    .join(library_directory)
                    .join("libonnxruntime.so");
                if bundled.exists() {
                    return bundled;
                }
            }

            #[cfg(target_os = "macos")]
            {
                let bundled = directory.join("../Frameworks/libonnxruntime.1.23.2.dylib");
                if bundled.exists() {
                    return bundled;
                }
            }

            #[cfg(target_os = "windows")]
            {
                let bundled = directory.join("onnxruntime.dll");
                if bundled.exists() {
                    return bundled;
                }
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    return PathBuf::from("libonnxruntime.so");

    #[cfg(target_os = "macos")]
    return PathBuf::from("libonnxruntime.1.23.2.dylib");

    #[cfg(target_os = "windows")]
    return PathBuf::from("onnxruntime.dll");
}
