//! Cross-platform "open URL in browser".

use std::process::{Command, Stdio};

/// Opens `url` in the user's default browser. Best-effort and non-fatal: errors
/// (e.g. no browser, headless) are swallowed, since the caller is expected to
/// have already printed the URL for manual opening. The child is detached so the
/// CLI does not block on it.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, use_shell) = ("open", false);
    #[cfg(target_os = "windows")]
    let (program, use_shell) = ("start", true);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let (program, use_shell) = ("xdg-open", false);

    let mut cmd = if use_shell {
        // `start` is a cmd.exe builtin on Windows, so it needs a shell.
        let mut c = Command::new("cmd");
        c.args(["/C", program, url]);
        c
    } else {
        let mut c = Command::new(program);
        c.arg(url);
        c
    };

    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
