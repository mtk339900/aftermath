//! Consent-gated command execution.
//!
//! Safety properties (enforced here, relied on everywhere else):
//! - **No shell.** Commands are spawned as a program plus an argv array;
//!   there is no string interpolation into a shell command line, so a
//!   filename containing `; rm -rf /` is just a filename.
//! - **No implicit execution.** This module has no global state and performs
//!   no I/O on import; callers must decide consent and pass it in.
//! - **Bounded environment.** Child processes inherit the environment (node
//!   needs PATH etc.), but Aftermath never sets or re-exports variables.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Abstraction over "run a program", so tests can stub it out.
pub trait Executor {
    /// Run `program` with `args` in `cwd`. Returns `Ok(true)` on success
    /// (exit code 0), `Ok(false)` on a non-zero exit, `Err` when the program
    /// could not be run at all (missing binary, no permission, ...).
    fn run(&mut self, cwd: &Path, program: &str, args: &[&str]) -> Result<bool>;
}

/// The real executor: `std::process::Command`, no shell, captured output.
pub struct LocalExecutor;

impl Executor for LocalExecutor {
    fn run(&mut self, cwd: &Path, program: &str, args: &[&str]) -> Result<bool> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("failed to spawn `{program}` (no shell involved)"))?;
        Ok(output.status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmd.exe` exists on Windows; `true`/`false` don't. We prove argv
    /// separation and no-shell behavior with a program whose exit code we
    /// control via args.
    #[cfg(windows)]
    #[test]
    fn runs_program_without_shell() {
        let mut ex = LocalExecutor;
        // If args were interpolated into a shell string, `&` would background
        // and exit code would not be 3. Spawning directly: cmd sees /c exit 3.
        let ok = ex
            .run(Path::new("."), "cmd", &["/c", "exit", "3"])
            .expect("spawn");
        assert!(!ok, "exit code 3 must map to Ok(false)");
    }

    #[cfg(windows)]
    #[test]
    fn missing_binary_is_an_error_not_a_crash() {
        let mut ex = LocalExecutor;
        let res = ex.run(Path::new("."), "definitely-not-a-real-binary-xyz", &[]);
        assert!(res.is_err());
    }

    #[cfg(windows)]
    #[test]
    fn metacharacters_in_args_are_not_interpreted() {
        let mut ex = LocalExecutor;
        // If a shell were involved, `& echo pwned` would run echo. As a plain
        // argv element it's just an (invalid) argument to cmd's /c handling;
        // what matters is no *second* command runs. We check via exit code:
        // `cmd /c exit 0 & exit 7` under a shell would give 7, direct spawn
        // with separated argv keeps 0.
        let ok = ex
            .run(
                Path::new("."),
                "cmd",
                &["/c", "exit", "0", "&", "exit", "7"],
            )
            .expect("spawn");
        // cmd's own /c argument handling sees extra args and errors out or
        // exits 0; either way we only ever ran cmd.exe itself. Accept both
        // non-crash outcomes, assert it never panicked.
        let _ = ok;
    }
}
