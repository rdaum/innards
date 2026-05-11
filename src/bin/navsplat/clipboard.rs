use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};

pub(super) fn copy_to_clipboard(text: &str) -> Result<String> {
    let commands: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    let mut failures = Vec::new();

    for (program, args) in commands {
        let mut child = match Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                failures.push(format!("{program}: failed to start: {err}"));
                continue;
            }
        };

        if let Some(mut stdin) = child.stdin.take()
            && let Err(err) = stdin.write_all(text.as_bytes())
        {
            failures.push(format!("{program}: failed to write selection: {err}"));
            continue;
        }
        match child.wait_with_output() {
            Ok(output) if output.status.success() => return Ok(program.to_string()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = stderr.trim();
                if message.is_empty() {
                    failures.push(format!("{program}: exited with {}", output.status));
                } else {
                    failures.push(format!("{program}: {message}"));
                }
            }
            Err(err) => failures.push(format!("{program}: failed to wait: {err}")),
        }
    }

    match copy_with_osc52(text) {
        Ok(()) => Ok("OSC 52".to_string()),
        Err(err) if failures.is_empty() => Err(err),
        Err(err) => Err(anyhow!(
            "clipboard tools failed ({}); OSC 52 failed: {err}",
            failures.join("; ")
        )),
    }
}

fn copy_with_osc52(text: &str) -> Result<()> {
    let payload = base64_encode(text.as_bytes());
    let sequence = if env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;\x1b\x1b]52;c;{payload}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{payload}\x07")
    };
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }

    output
}
