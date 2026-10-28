//! "Open in stelly-mix": a track handed to stelly-mix, the DJ workbench, as
//! `stelly-mix://track/<id>`. The id is the server's own, which is the one
//! stelly-mix mirrors Stelly's tracks under, so it opens the same track,
//! bringing its mix analysis over first if it has not yet.
//!
//! Only offered where stelly-mix has said it opens those links, which it does
//! each time it starts: a desktop that has it and has run it once. Anyone
//! else, the family on their phones and laptops, never sees the verb.

use std::process::Command;
use std::sync::OnceLock;

const SCHEME: &str = "stelly-mix";

/// Whether something opens `stelly-mix://` links here. Asked once: having
/// installed stelly-mix since is a restart away from showing up.
pub fn installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(handler_registered)
}

/// Hand track `id` to stelly-mix.
pub fn open(id: i64) {
    let link = format!("{SCHEME}://track/{id}");
    if let Err(err) = opener(&link).spawn() {
        eprintln!("could not open {link}: {err}");
    }
}

#[cfg(target_os = "linux")]
fn handler_registered() -> bool {
    Command::new("xdg-mime")
        .args(["query", "default", &format!("x-scheme-handler/{SCHEME}")])
        .output()
        .is_ok_and(|out| out.status.success() && !out.stdout.trim_ascii().is_empty())
}

#[cfg(windows)]
fn handler_registered() -> bool {
    hidden(Command::new("reg").args(["query", &format!(r"HKCU\Software\Classes\{SCHEME}"), "/ve"]))
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Neither Android nor macOS has a stelly-mix to hand anything to.
#[cfg(not(any(target_os = "linux", windows)))]
fn handler_registered() -> bool {
    false
}

#[cfg(windows)]
fn opener(link: &str) -> Command {
    let mut command = Command::new("explorer.exe");
    command.arg(link);
    command
}

#[cfg(not(windows))]
fn opener(link: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(link);
    command
}

#[cfg(windows)]
fn hidden(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW)
}
