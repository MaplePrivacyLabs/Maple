mod data;
mod run;
mod stats;

use anyhow::{Context, Result, bail, ensure};
use data::Scale;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
struct Options {
    full: bool,
    seed: bool,
    scale: Scale,
    output: PathBuf,
    mode: String,
    namespace: String,
    project: Option<String>,
}

fn main() -> Result<()> {
    // Lifecycle verification launches the current executable. Dispatch its
    // read-only checker before benchmark parsing or collecting measurements;
    // its authenticated key arrives only through the private stdin protocol.
    let mut checker_args = std::env::args_os().skip(1);
    if checker_args.next().as_deref()
        == Some(std::ffi::OsStr::new("--internal-store-integrity-check"))
    {
        let root = checker_args.next().context("Missing checker store root")?;
        ensure!(checker_args.next().is_none(), "Invalid checker arguments");
        return maple_agent::store::integrity::run_checker_stdio(&PathBuf::from(root))
            .map_err(Into::into);
    }
    maple_agent::store::integrity::register_checker_entry();
    let options = options()?;
    if options.seed {
        return run::seed_profile(&options);
    }
    let before = machine();
    let mut runs = Vec::new();
    if options.mode == "plain" || options.mode == "both" {
        runs.push(run::execute(run::Mode::Plain, &options)?);
    }
    if options.mode == "keyed" || options.mode == "both" {
        runs.push(run::execute(run::Mode::Keyed, &options)?);
    }
    let plans_pass = runs
        .iter()
        .all(|run| run.plans.iter().all(|plan| plan.passed));
    let report = json!({"format":"maple-store-bench/1","profile":if options.full {"full"}else{"ci"},
        "timestamp_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),"generator_seed":data::SEED,
        "measurement_boundary":"production Store APIs; durable source commits; fresh SQLite caches, warm OS cache; no UI/live-network latency",
        "machine_before":before,"machine_after":machine(),"runs":runs});
    let bytes = serde_json::to_vec_pretty(&report)?;
    if let Some(parent) = options.output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&options.output, &bytes)?;
    fs::write(
        options.output.with_extension("json.sha256"),
        format!(
            "{:x}  {}\n",
            Sha256::digest(&bytes),
            options
                .output
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ),
    )?;
    println!("{}", options.output.display());
    ensure!(plans_pass, "query-plan guard failed; inspect JSON evidence");
    Ok(())
}

fn options() -> Result<Options> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "store-bench [--full|--ci] [--mode both|plain|keyed] [--output FILE]\nstore-bench seed --tasks N --entries N --hot-task N [--project ROOT] [--namespace maple-agent[-dev|-prod]]\nMeasurements use synthetic temporary stores. Seed requires an absolute XDG_DATA_HOME and an existing signed-in plain new store; it refuses a running owner."
        );
        std::process::exit(0);
    }
    let full = args.iter().any(|arg| arg == "--full");
    let mut result = Options {
        full,
        seed: args.first().is_some_and(|arg| arg == "seed"),
        scale: if full { Scale::full() } else { Scale::ci() },
        output: PathBuf::from("store-bench.json"),
        mode: "both".into(),
        namespace: "maple-agent".into(),
        project: None,
    };
    let mut index = usize::from(result.seed);
    while index < args.len() {
        let name = &args[index];
        if name == "--full" || name == "--ci" {
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .with_context(|| format!("{name} needs a value"))?;
        match name.as_str() {
            "--tasks" => result.scale.tasks = value.parse()?,
            "--entries" => result.scale.entries = value.parse()?,
            "--hot-task" => result.scale.hot_entries = value.parse()?,
            "--long-task" => result.scale.long_entries = value.parse()?,
            "--output" => result.output = PathBuf::from(value),
            "--mode" => result.mode = value.clone(),
            "--project" => result.project = Some(value.clone()),
            "--namespace" => result.namespace = value.clone(),
            _ => bail!("Unknown option {name}"),
        }
        index += 2;
    }
    ensure!(
        ["plain", "keyed", "both"].contains(&result.mode.as_str()),
        "mode must be plain, keyed or both"
    );
    ensure!(
        ["maple-agent", "maple-agent-dev", "maple-agent-prod"].contains(&result.namespace.as_str()),
        "unrecognized profile namespace"
    );
    result.scale = result.scale.validate()?;
    Ok(result)
}

fn output(command: &str, args: &[&str]) -> Option<String> {
    let result = Command::new(command).args(args).output().ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}
fn physical_ram() -> Option<u64> {
    if cfg!(target_os = "macos") {
        output("sysctl", &["-n", "hw.memsize"])?.parse().ok()
    } else {
        let text = fs::read_to_string("/proc/meminfo").ok()?;
        text.lines()
            .find(|line| line.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    }
}
fn cpu_model() -> Option<String> {
    if cfg!(target_os = "macos") {
        output("sysctl", &["-n", "machdep.cpu.brand_string"])
    } else if cfg!(target_os = "windows") {
        std::env::var("PROCESSOR_IDENTIFIER").ok()
    } else {
        let text = fs::read_to_string("/proc/cpuinfo").ok()?;
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            matches!(key.trim(), "model name" | "Hardware").then(|| value.trim().to_owned())
        })
    }
}

fn peak_rss_bytes() -> Option<u64> {
    let peak = maple_agent::store::bench::process_peak_rss_bytes();
    if peak.is_some() || !cfg!(target_os = "windows") {
        return peak;
    }
    output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id()),
        ],
    )?
    .parse()
    .ok()
}

fn machine() -> Value {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
    json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"cpus":std::thread::available_parallelism().map(|n|n.get()).ok(),"cpu_model":cpu_model(),
        "process_peak_rss_bytes":peak_rss_bytes(),"rss_boundary":"process-lifetime high-water, including fixture generation, executable hashing and all modes; not per-scenario app RSS",
        "physical_ram_bytes":physical_ram(),"rustc":output("rustc",&["--version"]),"kernel":output("uname",&["-a"]),
        "load":output("uptime",&[]),"git_sha":output("git",&["rev-parse","HEAD"]),"git_dirty":output("git",&["status","--porcelain"]).map(|s|!s.is_empty()),
        "executable_sha256":binary,"runner_os":std::env::var("RUNNER_OS").ok(),"runner_arch":std::env::var("RUNNER_ARCH").ok()})
}
