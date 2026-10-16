use anyhow::{anyhow, Context, Result};
use std::ffi::OsStr;
use std::io::Write;
use std::process::{Command, Stdio};

/// Execution context: dry-run prints instead of running destructive commands.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub dry_run: bool,
}

impl Ctx {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }
}

/// Print `$ prog args...` in a shell-copy-pasteable way, then run it
/// (unless dry-run). Returns Ok(()) on exit status 0.
pub fn run<S: AsRef<OsStr>>(ctx: Ctx, prog: &str, args: &[S]) -> Result<()> {
    print_cmd(prog, args);
    if ctx.dry_run {
        println!("(dry-run: skipped)");
        return Ok(());
    }
    let status = Command::new(prog)
        .args(args)
        .stdin(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn `{prog}`"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("command `{prog}` exited with {status}"))
    }
}

/// Run and capture stdout (always executes, even in dry-run: read-only probe).
pub fn run_capture<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Result<String> {
    print_cmd(prog, args);
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to spawn `{prog}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "command `{prog}` exited with {}: {stderr}",
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pipe `input` into `prog args...` via stdin (used for chpasswd).
/// In dry-run, prints without executing.
pub fn run_with_stdin<S: AsRef<OsStr>>(
    ctx: Ctx,
    prog: &str,
    args: &[S],
    input: &str,
) -> Result<()> {
    print_cmd(prog, args);
    println!("(stdin: <redacted {} bytes>)", input.len());
    if ctx.dry_run {
        println!("(dry-run: skipped)");
        return Ok(());
    }
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn `{prog}`"))?;
    child
        .stdin
        .take()
        .context("failed to open stdin of child")?
        .write_all(input.as_bytes())
        .context("failed to write to child stdin")?;
    let status = child.wait().context("failed to wait for child")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("command `{prog}` exited with {status}"))
    }
}

fn print_cmd<S: AsRef<OsStr>>(prog: &str, args: &[S]) {
    let mut line = String::from("$ ");
    line.push_str(prog);
    for a in args {
        line.push(' ');
        let s = a.as_ref().to_string_lossy();
        if s.contains(' ') || s.contains('"') || s.is_empty() {
            line.push('"');
            line.push_str(&s.replace('"', "\\\""));
            line.push('"');
        } else {
            line.push_str(&s);
        }
    }
    println!("{line}");
}

/// True if `prog` resolves via PATH (`command -v` equivalent).
pub fn have(prog: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {}", shell_word(prog))])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
