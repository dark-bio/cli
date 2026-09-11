// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Local app execution, cancellation and terminal results.

use crate::progress::Transfer;
use crate::{Error, connect, find_enclave};
use console::style;
use darkbio_connect::schema::{ExecutionCancelRequest, ExecutionResultResponse};
use darkbio_connect::trust::Environment;
use darkbio_connect::{Client, Closer, ExecutionProgress, TrustMode};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(clap::Args)]
pub(super) struct Args {
    /// WASM app to upload and execute
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// Endpoint locator, or a unique serial, name or disk image
    #[arg(long)]
    device: Option<String>,

    /// Total budget in seconds for setup, upload, approval and execution
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
}

#[derive(clap::Args)]
pub(super) struct CancelArgs {
    /// Task ID printed by `ark execute`
    task: u64,

    /// Endpoint locator, or a unique serial, name or disk image
    #[arg(long)]
    device: Option<String>,

    /// Total budget in seconds for cloud setup and cancellation
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
}

pub(super) fn run(args: Args, env: Option<Environment>) -> Result<(), Error> {
    let (mut file, size) = open(&args.file)?;
    let endpoint = find_enclave(args.device.as_deref())?;
    let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
    let client = ark.client();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(args.timeout))
        .ok_or_else(|| "execution timeout is too large".to_owned())?;
    let task = Arc::new(Mutex::new(None));
    interrupt(client.clone(), ark.closer(), task.clone())?;
    eprintln!("Executing {}", style(args.file.display()).bold());
    let mut transfer = Transfer::default();
    let mut reported = None;
    let result = client.execute(size, &mut file, deadline, |stage| match stage {
        ExecutionProgress::Preparing => eprintln!("{}", style("Preparing app upload…").dim()),
        ExecutionProgress::Started { taskid } => {
            *task.lock().expect("task not poisoned") = Some(taskid);
            eprintln!("{}", style(format!("Task {taskid}")).dim());
        }
        ExecutionProgress::Uploading { uploaded, total } => {
            if let Some(line) = transfer.update(uploaded, total) {
                eprintln!("{}", style(line).dim());
            }
        }
        ExecutionProgress::Authorizing => {
            eprintln!(
                "{}",
                style("Approve execution in your companion app.").dim()
            );
        }
        ExecutionProgress::Running { elapsed } => {
            let seconds = elapsed.as_secs();
            if reported.is_none_or(|last| seconds >= last + 5) {
                eprintln!("{}", style(format!("Running: {seconds}s elapsed")).dim());
                reported = Some(seconds);
            }
        }
    });
    *task.lock().expect("task not poisoned") = None;
    let result = result?;
    output(&result, &mut io::stdout().lock(), &mut io::stderr().lock())?;
    if !result.success {
        return Err(Error {
            code: 4,
            message: "app execution failed".into(),
        });
    }
    eprintln!(
        "{}",
        style(format!(
            "{} {} completed successfully.",
            result.app_name, result.app_version
        ))
        .green()
    );
    Ok(())
}

pub(super) fn cancel(args: CancelArgs, env: Option<Environment>) -> Result<(), Error> {
    let endpoint = find_enclave(args.device.as_deref())?;
    let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
    ark.client().call_timeout(
        ExecutionCancelRequest { taskid: args.task },
        Duration::from_secs(args.timeout),
    )?;
    println!(
        "{}",
        style(format!("Task {} cancelled.", args.task)).green()
    );
    Ok(())
}

fn open(path: &Path) -> Result<(File, u64), Error> {
    let file =
        File::open(path).map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()).into());
    }
    if metadata.len() == 0 {
        return Err(format!("{} is empty", path.display()).into());
    }
    Ok((file, metadata.len()))
}

/// App output may be binary. Preserve both streams exactly, including absent
/// trailing newlines, and keep all CLI diagnostics on stderr.
fn output(
    result: &ExecutionResultResponse,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), Error> {
    stdout
        .write_all(&result.stdout)
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("failed to write app stdout: {error}"))?;
    stderr
        .write_all(&result.stderr)
        .and_then(|()| stderr.flush())
        .map_err(|error| format!("failed to write app stderr: {error}"))?;
    Ok(())
}

/// Ctrl-C runs on ctrlc's worker thread, so it can cancel while the command is
/// waiting for approval or a response. The library installs no signal handlers.
fn interrupt(client: Client, closer: Closer, task: Arc<Mutex<Option<u64>>>) -> Result<(), Error> {
    ctrlc::set_handler(move || {
        // Retain the guard until exit so a cancellation response cannot let
        // the main thread report completion before the handler finishes.
        let task = task.lock().expect("task not poisoned");
        if let Some(taskid) = *task {
            eprintln!("{}", style(format!("Cancelling task {taskid}…")).dim());
            match client.call_timeout(ExecutionCancelRequest { taskid }, Duration::from_secs(5)) {
                Ok(_) => eprintln!("{}", style(format!("Task {taskid} cancelled.")).yellow()),
                Err(error) => eprintln!(
                    "{}",
                    style(format!(
                        "Could not confirm cancellation of task {taskid}: {error}"
                    ))
                    .red()
                ),
            }
        } else {
            eprintln!(
                "{}",
                style("Interrupted without an active task ID.").yellow()
            );
        }
        closer.close();
        std::process::exit(130);
    })
    .map_err(|error| format!("failed to install Ctrl-C handler: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_arguments() {
        assert!(
            crate::Cli::try_parse_from(["ark", "execute", "app.wasm", "--timeout", "20"]).is_ok()
        );
        assert!(crate::Cli::try_parse_from(["ark", "cancel", "18446744073709551615"]).is_ok());
        for args in [
            vec!["ark", "execute"],
            vec!["ark", "execute", "app.wasm", "--timeout", "0"],
            vec!["ark", "cancel"],
            vec!["ark", "cancel", "oops"],
            vec!["ark", "cancel", "1", "--timeout", "0"],
        ] {
            assert!(crate::Cli::try_parse_from(args).is_err());
        }
    }

    /// Non-UTF-8 output and missing newlines survive both successful and failed
    /// app results. Progress and completion messages never enter stdout.
    #[test]
    fn test_output() {
        for success in [true, false] {
            let result = ExecutionResultResponse {
                success,
                stdout: vec![0, 255, 42],
                stderr: vec![254, 0],
                ..Default::default()
            };
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            output(&result, &mut stdout, &mut stderr).unwrap();
            assert_eq!(stdout, result.stdout);
            assert_eq!(stderr, result.stderr);
        }
        let error = output(
            &ExecutionResultResponse {
                stdout: vec![42],
                ..Default::default()
            },
            &mut [].as_mut_slice(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.message.contains("failed to write app stdout"));
    }
}
