//! Running the programs sandboxes are driven through.

use crate::failure;
use cuma_core::error::Result;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// What a program reads on stdin.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Input<'a> {
    /// Nothing: stdin is closed.
    Nothing,
    /// These bytes.
    Bytes(&'a [u8]),
    /// This file.
    File(&'a Path),
}

/// Where a program's stdout goes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Output<'a> {
    /// Returned, as text.
    Capture,
    /// Into this file.
    File(&'a Path),
}

/// Run `program` with `args`, for `sandbox`. Returns its stdout when it
/// exits successfully; otherwise an error naming the command and quoting the
/// end of its stderr. Killed when `timeout` passes.
///
/// Arguments are shown in errors: nothing secret is ever placed in them.
pub(crate) async fn run(
    sandbox: &str,
    program: &str,
    args: &[String],
    input: Input<'_>,
    output: Output<'_>,
    timeout: Duration,
) -> Result<String> {
    let shown = || {
        let mut words = vec![program.to_owned()];
        words.extend(args.iter().cloned());
        let mut line = shell_words::join(words);
        if line.len() > 300 {
            let cut = (0..=300)
                .rev()
                .find(|i| line.is_char_boundary(*i))
                .unwrap_or(0);
            line.truncate(cut);
            line.push('…');
        }
        line
    };

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .kill_on_drop(true)
        .stderr(Stdio::piped())
        // macOS tar would otherwise add `._*` resource-fork files to archives.
        .env("COPYFILE_DISABLE", "1");
    command.stdin(match input {
        Input::Nothing => Stdio::null(),
        Input::Bytes(_) => Stdio::piped(),
        Input::File(path) => Stdio::from(
            std::fs::File::open(path)
                .map_err(|e| failure(sandbox, format!("cannot read {}: {e}", path.display())))?,
        ),
    });
    command.stdout(match output {
        Output::Capture => Stdio::piped(),
        Output::File(path) => Stdio::from(
            std::fs::File::create(path)
                .map_err(|e| failure(sandbox, format!("cannot write {}: {e}", path.display())))?,
        ),
    });

    let mut child = command
        .spawn()
        .map_err(|e| failure(sandbox, format!("cannot run {program}: {e}")))?;
    if let (Input::Bytes(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(bytes)
            .await
            .map_err(|e| failure(sandbox, format!("{}: writing its input: {e}", shown())))?;
    }

    let finished = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            failure(
                sandbox,
                format!("{} timed out after {}s", shown(), timeout.as_secs()),
            )
        })?
        .map_err(|e| failure(sandbox, format!("{}: {e}", shown())))?;
    if finished.status.success() {
        return Ok(String::from_utf8_lossy(&finished.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&finished.stderr);
    let stderr = stderr.trim();
    let tail_start = stderr
        .char_indices()
        .rev()
        .nth(1500)
        .map_or(0, |(index, _)| index);
    Err(failure(
        sandbox,
        format!(
            "{} failed ({}): {}",
            shown(),
            finished.status,
            if stderr.is_empty() {
                "no output"
            } else {
                &stderr[tail_start..]
            }
        ),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[tokio::test]
    async fn stdout_is_returned_and_stdin_is_fed() {
        let out = run(
            "t",
            "sh",
            &args(&["-c", "cat; echo done"]),
            Input::Bytes(b"in "),
            Output::Capture,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out, "in done\n");
    }

    #[tokio::test]
    async fn a_failure_names_the_command_and_quotes_its_stderr() {
        let err = run(
            "box",
            "sh",
            &args(&["-c", "echo nope >&2; exit 3"]),
            Input::Nothing,
            Output::Capture,
            Duration::from_secs(10),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("sandbox box"), "{err}");
        assert!(err.contains("sh -c"), "{err}");
        assert!(err.contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn a_program_that_hangs_is_killed_at_the_timeout() {
        let started = std::time::Instant::now();
        let err = run(
            "t",
            "sleep",
            &args(&["30"]),
            Input::Nothing,
            Output::Capture,
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
