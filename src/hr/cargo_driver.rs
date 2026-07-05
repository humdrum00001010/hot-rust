//! Cargo execution backend.
//!
//! Cargo does not drive live reload decisions. In live mode this module builds
//! or launches the target, then returns RA work to `RustAnalyzerDriver`.

use serde_json::Value;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::live::LiveConfig;
use super::session::HotSession;
use super::util::{cargo_command, env_flag, log_timing};
use super::{PATCHABLE_ENTRY_FLAG, PATCH_BUILD_ONLY_ENV};

pub(crate) enum CargoDriverResult {
    Completed,
    LiveBuildOnly(LiveBuildRequest),
    LiveTarget(LiveTargetRun),
}

pub(crate) struct LiveBuildRequest {
    pub(crate) executable: PathBuf,
    pub(crate) live: LiveConfig,
    pub(crate) cargo_side: Vec<String>,
    pub(crate) bin_name: Option<String>,
}

pub(crate) struct LiveTargetRun {
    pub(crate) executable: PathBuf,
    pub(crate) live: LiveConfig,
    pub(crate) cargo_side: Vec<String>,
    pub(crate) bin_name: Option<String>,
    pub(crate) child: Child,
}

pub(crate) fn run_cargo(
    workspace_root: &Path,
    session: &HotSession,
    cargo_args: &[String],
) -> Result<CargoDriverResult, Box<dyn Error>> {
    if cargo_args.first().map(String::as_str) == Some("run") {
        return run_cargo_run(workspace_root, session, &cargo_args[1..]);
    }

    println!(
        "hr: cargo {} with {}",
        cargo_args.join(" "),
        PATCHABLE_ENTRY_FLAG
    );
    let mut command = Command::new(cargo_command());
    command.args(cargo_args).current_dir(workspace_root);
    session.apply_env(&mut command, workspace_root);

    let status = command.status()?;
    if !status.success() {
        return Err(format!("cargo exited with {status}").into());
    }

    Ok(CargoDriverResult::Completed)
}

fn run_cargo_run(
    workspace_root: &Path,
    session: &HotSession,
    run_args: &[String],
) -> Result<CargoDriverResult, Box<dyn Error>> {
    let (cargo_side, binary_args) = split_run_args(run_args);
    if cargo_side
        .iter()
        .any(|arg| arg == "--message-format" || arg.starts_with("--message-format="))
    {
        return Err("hr cargo run reserves --message-format so it can find the executable".into());
    }

    let mut build_args = Vec::with_capacity(cargo_side.len() + 3);
    build_args.push("build".to_string());
    build_args.extend(cargo_side.iter().cloned());
    build_args.push("--message-format=json-render-diagnostics".to_string());
    let bin_name = selected_bin_name(cargo_side);

    println!(
        "hr: translating cargo run -> cargo {}",
        build_args.join(" ")
    );
    let executable = cargo_build_executable(workspace_root, session, &build_args)?;
    println!("hr: executable {}", executable.display());

    let mut child = Command::new(&executable);
    child.args(binary_args).current_dir(workspace_root);
    session.apply_env(&mut child, workspace_root);

    let live = LiveConfig::from_env()?;
    if let Some(live) = live {
        if env_flag(PATCH_BUILD_ONLY_ENV) {
            return Ok(CargoDriverResult::LiveBuildOnly(LiveBuildRequest {
                executable,
                live,
                cargo_side: cargo_side.to_vec(),
                bin_name,
            }));
        }
        live.apply_runtime_env(&mut child)?;
        println!("hr: launching {}", executable.display());
        let child = child.spawn()?;
        return Ok(CargoDriverResult::LiveTarget(LiveTargetRun {
            executable,
            live,
            cargo_side: cargo_side.to_vec(),
            bin_name,
            child,
        }));
    }

    println!("hr: launching {}", executable.display());
    let status = child.status()?;
    if !status.success() {
        return Err(format!("target exited with {status}").into());
    }

    Ok(CargoDriverResult::Completed)
}
fn cargo_build_executable(
    workspace_root: &Path,
    session: &HotSession,
    build_args: &[String],
) -> Result<PathBuf, Box<dyn Error>> {
    let start = Instant::now();
    let mut command = Command::new(cargo_command());
    command
        .args(build_args)
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    session.apply_env(&mut command, workspace_root);

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or("failed to capture cargo JSON stdout")?;
    let reader = BufReader::new(stdout);
    let mut executable = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(value) => {
                if let Some(rendered) = value
                    .get("message")
                    .and_then(|message| message.get("rendered"))
                    .and_then(Value::as_str)
                {
                    eprint!("{rendered}");
                }

                if value.get("reason").and_then(Value::as_str) == Some("compiler-artifact") {
                    if let Some(path) = value.get("executable").and_then(Value::as_str) {
                        executable = Some(PathBuf::from(path));
                    }
                }
            }
            Err(_) => println!("{line}"),
        }
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("cargo build exited with {status}").into());
    }
    log_timing("target-cargo-build", start);

    executable.ok_or_else(|| "cargo build did not report an executable artifact".into())
}
fn split_run_args(args: &[String]) -> (&[String], &[String]) {
    if let Some(index) = args.iter().position(|arg| arg == "--") {
        (&args[..index], &args[index + 1..])
    } else {
        (args, &[])
    }
}

fn selected_bin_name(cargo_side: &[String]) -> Option<String> {
    let mut args = cargo_side.iter();
    while let Some(arg) = args.next() {
        if arg == "--bin" {
            return args.next().cloned();
        }
        if let Some(name) = arg.strip_prefix("--bin=") {
            return Some(name.to_string());
        }
    }
    None
}
