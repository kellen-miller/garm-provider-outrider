//! Visible host-tool execution with ordered, merged child output.
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Seek},
    process::{Command, Stdio},
};

/// Execute a tool, reporting its command and retaining merged output order.
/// # Errors
/// Returns spawn, output, or unsuccessful-exit errors.
pub fn run(command: &mut Command) -> Result<String> {
    println!(
        "+ {} {:?}",
        command.get_program().to_string_lossy(),
        command.get_args().collect::<Vec<_>>()
    );
    let mut output = tempfile::tempfile()?;
    command
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output.try_clone()?));
    let status = command.status().context("execute host tool")?;
    output.rewind()?;
    let mut text = String::new();
    output.read_to_string(&mut text)?;
    print!("{text}");
    if !text.is_empty() && !text.ends_with('\n') {
        println!();
    }

    ensure!(
        status.success(),
        "host tool exited {status}: {}",
        command.get_program().to_string_lossy()
    );
    Ok(text)
}
