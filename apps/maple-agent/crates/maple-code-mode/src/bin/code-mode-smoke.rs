//! Debug/package acceptance probe. Uses the shipped resolver and framed worker.
use maple_code_mode::{Config, LaunchSpec, OutcomeStatus, PackagedPython, Runtime};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let python = match args.as_slice() {
        [flag, path] if flag == "--manifest" => PackagedPython::from_manifest(path)?,
        [] => match std::env::var_os("MAPLE_CODE_MODE_RUNTIME_MANIFEST") {
            Some(path) => PackagedPython::from_manifest(path)?,
            None => PackagedPython::for_application_executable(std::env::current_exe()?)?,
        },
        _ => return Err("usage: code-mode-smoke [--manifest PATH]".into()),
    };
    let directory = std::env::temp_dir().join(format!(
        "Maple Python smoke é {}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir(&directory)?;
    let scratch = Scratch(directory.canonicalize()?);
    let runtime = Runtime::new(Config::default());
    let task = runtime.bind(
        "smoke",
        LaunchSpec {
            python,
            cwd: scratch.0.clone(),
            env: std::env::vars_os().collect(),
        },
        CancellationToken::new(),
    )?;
    let probe = async {
        let first = task.execute(
            "import ssl, sqlite3, ctypes, zlib, bz2, lzma, asyncio, __main__\nassert __main__.__dict__ is globals()\nloop = asyncio.get_running_loop()\nanswer = 40\nprint('bundled stdlib imports passed')",
            CancellationToken::new(),
        ).await?;
        if first.status != OutcomeStatus::Ok {
            return Err(format!("stdlib probe failed: {:?}", first.traceback).into());
        }
        let second = task
            .execute(
                "assert asyncio.get_running_loop() is loop\nawait asyncio.sleep(0)\nanswer + 2",
                CancellationToken::new(),
            )
            .await?;
        if second.status != OutcomeStatus::Ok || second.value.as_deref() != Some("42") {
            return Err(format!("persistent async probe failed: {:?}", second.traceback).into());
        }
        let identity = first
            .runtime
            .ok_or("worker did not report its runtime identity")?;
        println!(
            "CPython {} ({}) at {}: native worker smoke passed",
            identity.version,
            identity.distribution,
            identity.executable.display()
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(30), probe).await;
    runtime.shutdown("smoke complete").await?;
    result??;
    Ok(())
}
