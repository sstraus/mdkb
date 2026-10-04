//! Helpers shared by unit tests in more than one module.

use std::path::Path;

/// What the stub does with the prompt on stdin.
pub(crate) enum StubStdin {
    /// Never read, so the parent's write meets a closed pipe.
    Ignored,
    /// Read to EOF and discarded, the shape of a CLI that consumes its prompt.
    Drained,
    /// Read to EOF and written back out, byte for byte.
    Echoed,
}

/// What the stub does with the prompt when it arrives in argv instead.
pub(crate) enum StubArgv {
    /// No placeholder: the prompt stays on stdin.
    Ignored,
    /// Appended to stdout, so a test can see which value arrived.
    Echoed,
    /// Nothing is printed unless an argument arrived.
    Required,
}

/// A stub distiller: a script that `run_distiller_cli` really spawns.
///
/// The body is written per platform — `sh` on Unix, PowerShell on Windows —
/// rather than gated to Unix, because the spawn itself is what these tests
/// cover and it is where the two platforms differ most. The canned output
/// travels in files beside the script instead of inside the command line: a
/// JSON answer does not survive `cmd.exe`, which does not undo the
/// MSVCRT-style quote escaping Rust applies to arguments.
pub(crate) struct Stub<'a> {
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub exit: i32,
    pub stdin: StubStdin,
    pub argv: StubArgv,
}

impl Default for Stub<'_> {
    fn default() -> Self {
        Self {
            stdout: "",
            stderr: "",
            exit: 0,
            stdin: StubStdin::Drained,
            argv: StubArgv::Ignored,
        }
    }
}

impl Stub<'_> {
    /// Write the stub into `dir` and return the `(program, args)` pair to hand
    /// to the distiller under test.
    pub(crate) fn build(&self, dir: &Path) -> (String, Vec<String>) {
        std::fs::write(dir.join("stub_stdout"), self.stdout).expect("stub stdout must be written");
        std::fs::write(dir.join("stub_stderr"), self.stderr).expect("stub stderr must be written");

        let (program, mut args) = if cfg!(windows) {
            self.write_powershell(dir)
        } else {
            self.write_sh(dir)
        };
        if !matches!(self.argv, StubArgv::Ignored) {
            args.push("{prompt}".to_string());
        }
        (program, args)
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn write_sh(&self, dir: &Path) -> (String, Vec<String>) {
        let mut body = String::from("#!/bin/sh\nd=$(dirname \"$0\")\n");
        match self.stdin {
            StubStdin::Ignored => {}
            StubStdin::Drained => body.push_str("cat >/dev/null\n"),
            StubStdin::Echoed => body.push_str("cat\n"),
        }
        if matches!(self.argv, StubArgv::Required) {
            body.push_str(&format!("[ -n \"$1\" ] || exit {}\n", self.exit));
        }
        body.push_str("cat \"$d/stub_stdout\"\n");
        if matches!(self.argv, StubArgv::Echoed) {
            body.push_str("printf '%s' \"$1\"\n");
        }
        body.push_str("cat \"$d/stub_stderr\" >&2\n");
        body.push_str(&format!("exit {}\n", self.exit));

        let path = dir.join("stub.sh");
        std::fs::write(&path, body).expect("stub script must be written");
        // Run it as `sh stub.sh`, never as `./stub.sh`: exec of a file that was
        // just written fails with ETXTBSY whenever another test thread forked
        // while the write fd was open, because that child holds a copy of the
        // fd until its own exec. Interpreting the script only reads it.
        ("sh".to_string(), vec![path.to_string_lossy().into_owned()])
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn write_powershell(&self, dir: &Path) -> (String, Vec<String>) {
        // `[Console]` reads and writes the raw streams: `Write-Host` and the
        // pipeline would append a line ending the Unix side does not add, and
        // tests compare stdout byte for byte.
        let mut body = String::from("param([string]$Prompt)\n");
        match self.stdin {
            StubStdin::Ignored => {}
            StubStdin::Drained => body.push_str("$null = [Console]::In.ReadToEnd()\n"),
            StubStdin::Echoed => body.push_str("[Console]::Out.Write([Console]::In.ReadToEnd())\n"),
        }
        if matches!(self.argv, StubArgv::Required) {
            body.push_str(&format!("if (-not $Prompt) {{ exit {} }}\n", self.exit));
        }
        body.push_str(
            "[Console]::Out.Write([IO.File]::ReadAllText(\"$PSScriptRoot\\stub_stdout\"))\n",
        );
        if matches!(self.argv, StubArgv::Echoed) {
            body.push_str("[Console]::Out.Write($Prompt)\n");
        }
        body.push_str(
            "[Console]::Error.Write([IO.File]::ReadAllText(\"$PSScriptRoot\\stub_stderr\"))\n",
        );
        body.push_str(&format!("exit {}\n", self.exit));

        let path = dir.join("stub.ps1");
        std::fs::write(&path, body).expect("stub script must be written");
        (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                path.to_string_lossy().into_owned(),
            ],
        )
    }
}

/// Keep a paused Tokio clock under explicit control while real sockets are idle.
/// A blocking task inhibits Tokio's automatic jump to the next timeout without
/// consuming CPU. Dropping the guard releases it, including during a panic.
pub(crate) struct ManualClock {
    _release: std::sync::mpsc::Sender<()>,
}

impl ManualClock {
    pub(crate) fn new() -> Self {
        let (release, wait) = std::sync::mpsc::channel();
        tokio::task::spawn_blocking(move || {
            let _ = wait.recv();
        });
        Self { _release: release }
    }
}
