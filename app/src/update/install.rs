//! Getting a release onto this machine: which file this install updates
//! from, downloading and checking it, and putting it in place.
//!
//! Every desktop path ends the same way, the new version started and this
//! process gone. Android cannot be updated from inside: the APK goes to the
//! system's installer, which asks the user and then replaces the app.

use super::{Download, Release, USER_AGENT};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// How long a download may go without a byte before it is given up on.
const STALL: Duration = Duration::from_secs(30);

/// How this copy of the app got here, which decides what it updates from and
/// how.
#[derive(Clone, Debug, PartialEq)]
pub enum Install {
    /// Replaced in place, which is how AppImages update.
    AppImage { path: PathBuf, ubuntu: &'static str },
    /// Under /usr, so root's business: apt installs the new .deb through
    /// pkexec.
    Deb { exe: PathBuf, ubuntu: &'static str },
    /// Unpacked from the tarball somewhere this user can write.
    Tarball { exe: PathBuf, ubuntu: &'static str },
    /// The setup.exe's per-user install, which upgrades without UAC.
    Nsis { exe: PathBuf },
    Msi { exe: PathBuf },
    Apk,
    /// Built from source, or somewhere this cannot write.
    Manual,
}

impl Install {
    /// Worked out once, on the first release check: once a .deb has replaced
    /// the binary, `current_exe` on Linux reads `/usr/bin/stelly (deleted)`.
    pub fn current() -> &'static Install {
        static CURRENT: OnceLock<Install> = OnceLock::new();
        CURRENT.get_or_init(detect)
    }

    /// The release asset this install updates from, as the release job names
    /// them.
    pub(super) fn asset(&self, version: &str) -> Option<String> {
        Some(match self {
            Install::AppImage { ubuntu, .. } => format!("stelly_{version}_ubuntu-{ubuntu}.AppImage"),
            Install::Deb { ubuntu, .. } => format!("stelly_{version}_amd64.ubuntu-{ubuntu}.deb"),
            Install::Tarball { ubuntu, .. } => format!("stelly_{version}_ubuntu-{ubuntu}.tar.gz"),
            Install::Nsis { .. } => format!("stelly_{version}_x64-setup.exe"),
            Install::Msi { .. } => format!("stelly_{version}_x64.msi"),
            Install::Apk => format!("stelly_{version}_arm64.apk"),
            Install::Manual => return None,
        })
    }
}

fn detect() -> Install {
    // Someone working on the app. The tarball case would otherwise happily
    // overwrite target/debug with a release, and a debug APK is signed with a
    // key the release cannot replace.
    if cfg!(debug_assertions) {
        return Install::Manual;
    }
    if cfg!(target_os = "android") {
        return Install::Apk;
    }
    let Ok(exe) = std::env::current_exe() else {
        return Install::Manual;
    };
    if cfg!(windows) {
        windows(exe)
    } else if cfg!(target_os = "linux") {
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        linux(exe, std::env::var_os("APPIMAGE").map(PathBuf::from), &os_release)
    } else {
        Install::Manual
    }
}

fn linux(exe: PathBuf, appimage: Option<PathBuf>, os_release: &str) -> Install {
    let ubuntu = ubuntu_build(os_release);
    // Set by the AppImage runtime, and the only way to find the file: `exe` is
    // inside its mount.
    if let Some(path) = appimage {
        // The build it was downloaded as, while the name still says.
        let named = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| ["24.04", "26.04"].into_iter().find(|v| name.contains(&format!("ubuntu-{v}"))));
        return Install::AppImage {
            ubuntu: named.unwrap_or(ubuntu),
            path,
        };
    }
    if exe.starts_with("/usr") {
        return Install::Deb { exe, ubuntu };
    }
    if exe.parent().is_some_and(writable) {
        return Install::Tarball { exe, ubuntu };
    }
    Install::Manual
}

/// Which of the two Ubuntu builds suits this system. 26.04's only on 26.04
/// and later; everything else, other distributions included, gets 24.04's,
/// whose older glibc runs in more places.
fn ubuntu_build(os_release: &str) -> &'static str {
    let field = |key: &str| {
        os_release.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?;
            Some(value.trim().trim_matches('"'))
        })
    };
    let recent = field("ID") == Some("ubuntu")
        && field("VERSION_ID")
            .and_then(super::parse)
            .is_some_and(|version| version >= (26, 4, 0));
    if recent {
        "26.04"
    } else {
        "24.04"
    }
}

fn windows(exe: PathBuf) -> Install {
    let Some(dir) = exe.parent() else {
        return Install::Manual;
    };
    // The setup.exe leaves its uninstaller next to the app; the MSI does not.
    if dir.join("uninstall.exe").is_file() {
        return Install::Nsis { exe };
    }
    let in_program_files = std::env::var_os("ProgramFiles").is_some_and(|dir| exe.starts_with(dir));
    if in_program_files {
        return Install::Msi { exe };
    }
    Install::Manual
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".stelly-update-probe");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// What happened, when installing did not end this process.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Done {
    /// The system's installer has it from here: Android's prompt, or the
    /// software centre the .deb was opened in.
    HandedOff,
    /// Android wants this app allowed to install others first, and its
    /// settings page for that is open.
    NeedsPermission,
}

/// Download `release` and install it. `progress` hears bytes so far and the
/// total. On desktop, success does not return: the new version is started and
/// this process exits.
pub async fn install(release: &Release, mut progress: impl FnMut(u64, u64)) -> Result<Done> {
    let install = Install::current();
    let file = release
        .file
        .as_ref()
        .context("this install cannot update itself")?;

    // Whatever an earlier update left behind goes first.
    let dir = crate::client_data_dir().join("update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    // Next to the AppImage rather than in the data directory, so putting it
    // in place is a rename within one file system.
    let target = match install {
        Install::AppImage { path, .. } => with_suffix(path, ".new"),
        _ => dir.join(&file.name),
    };
    if let Err(err) = download(file, &target, &mut progress).await {
        let _ = std::fs::remove_file(&target);
        return Err(err);
    }
    tokio::task::spawn_blocking(move || apply(install, &target, &dir)).await?
}

async fn download(file: &Download, to: &Path, progress: &mut impl FnMut(u64, u64)) -> Result<()> {
    let response = crate::backend::remote::http_client()
        .get(&file.url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?
        .error_for_status()?;
    let total = response.content_length().unwrap_or(file.size);

    let mut out = std::fs::File::create(to).with_context(|| format!("could not write {}", to.display()))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(STALL, stream.next())
        .await
        .context("the download stalled")?
    {
        let chunk = chunk?;
        out.write_all(&chunk)?;
        hasher.update(&chunk);
        done += chunk.len() as u64;
        progress(done, total);
    }
    out.sync_all()?;

    if file.size != 0 && done != file.size {
        bail!("the download stopped at {done} of {} bytes", file.size);
    }
    if let Some(expected) = &file.sha256 {
        let actual: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        if &actual != expected {
            bail!("the download does not match GitHub's checksum");
        }
    }
    Ok(())
}

fn apply(install: &Install, file: &Path, dir: &Path) -> Result<Done> {
    match install {
        Install::AppImage { path, .. } => {
            set_executable(file)?;
            std::fs::rename(file, path).with_context(|| format!("could not replace {}", path.display()))?;
            restart(path)
        }
        Install::Tarball { exe, .. } => {
            let unpacked = dir.join("unpacked");
            std::fs::create_dir_all(&unpacked)?;
            let status = Command::new("tar")
                .arg("-xzf")
                .arg(file)
                .arg("-C")
                .arg(&unpacked)
                .arg("--strip-components=1")
                .status()
                .context("could not run tar")?;
            if !status.success() {
                bail!("tar could not unpack {}", file.display());
            }
            // Copied in beside the old binary first, so the swap itself is a
            // rename and the data directory can sit on another file system.
            let staged = with_suffix(exe, ".new");
            std::fs::copy(unpacked.join("stelly"), &staged)?;
            set_executable(&staged)?;
            std::fs::rename(&staged, exe).with_context(|| format!("could not replace {}", exe.display()))?;
            restart(exe)
        }
        Install::Deb { exe, .. } => {
            let output = Command::new("pkexec")
                .args(["apt-get", "install", "-y", "-o", "DPkg::Lock::Timeout=60"])
                .arg(file)
                .output();
            match output {
                // No polkit: the software centre can install it instead.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    open(file)?;
                    Ok(Done::HandedOff)
                }
                Err(err) => Err(err).context("could not run pkexec"),
                Ok(output) if output.status.success() => restart(exe),
                Ok(output) if matches!(output.status.code(), Some(126 | 127)) => {
                    bail!("installing needs an administrator's password")
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let reason = stderr.lines().rev().find(|line| !line.trim().is_empty());
                    bail!("apt-get failed: {}", reason.unwrap_or("no reason given"))
                }
            }
        }
        Install::Nsis { exe } => {
            let into = exe.parent().context("the app has no directory")?;
            // NSIS wants /D last and unquoted, spaces and all, and
            // Start-Process passes it through as written.
            let run = format!(
                "Start-Process -FilePath {} -ArgumentList '/S',{} -Wait",
                powershell_quote(&file.display().to_string()),
                powershell_quote(&format!("/D={}", into.display())),
            );
            after_exit(&run, exe, dir)
        }
        Install::Msi { exe } => {
            let run = format!(
                "Start-Process -FilePath msiexec.exe -ArgumentList '/i',{},'/passive','/norestart' -Wait",
                powershell_quote(&format!("\"{}\"", file.display())),
            );
            after_exit(&run, exe, dir)
        }
        #[cfg(target_os = "android")]
        Install::Apk => super::android::install(file),
        #[cfg(not(target_os = "android"))]
        Install::Apk => unreachable!("only Android builds detect an APK install"),
        Install::Manual => bail!("this install cannot update itself"),
    }
}

/// Start the version now on disk and leave.
fn restart(exe: &Path) -> Result<Done> {
    Command::new(exe)
        .spawn()
        .context("updated, but could not start the new version")?;
    std::process::exit(0);
}

/// Windows will not overwrite a running exe, so the installer runs from a
/// hidden PowerShell that waits for this process to be gone first, then
/// starts the new version.
fn after_exit(run: &str, exe: &Path, dir: &Path) -> Result<Done> {
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue\r\n{run}\r\nStart-Process -FilePath {}\r\n",
        std::process::id(),
        powershell_quote(&exe.display().to_string()),
    );
    let path = dir.join("update.ps1");
    std::fs::write(&path, script)?;

    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"])
        .args(["-WindowStyle", "Hidden", "-File"])
        .arg(&path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().context("could not start the installer")?;
    std::process::exit(0);
}

fn powershell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn open(file: &Path) -> Result<()> {
    Command::new("xdg-open")
        .arg(file)
        .spawn()
        .context("could not open the package")?;
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU_2604: &str = "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\nVERSION_ID=\"26.04\"\n";
    const UBUNTU_2404: &str = "ID=ubuntu\nVERSION_ID=\"24.04\"\n";
    const FEDORA: &str = "ID=fedora\nVERSION_ID=44\n";

    #[test]
    fn picks_the_ubuntu_build() {
        assert_eq!(ubuntu_build(UBUNTU_2604), "26.04");
        assert_eq!(ubuntu_build(UBUNTU_2404), "24.04");
        assert_eq!(ubuntu_build(FEDORA), "24.04");
        assert_eq!(ubuntu_build(""), "24.04");
    }

    #[test]
    fn an_appimage_keeps_the_build_it_came_as() {
        let install = linux(
            PathBuf::from("/tmp/.mount_stellyXYZ/usr/bin/stelly"),
            Some(PathBuf::from("/home/me/Apps/stelly_0.16.2_ubuntu-24.04.AppImage")),
            UBUNTU_2604,
        );
        assert_eq!(
            install,
            Install::AppImage {
                path: PathBuf::from("/home/me/Apps/stelly_0.16.2_ubuntu-24.04.AppImage"),
                ubuntu: "24.04",
            }
        );
    }

    #[test]
    fn under_usr_is_the_deb() {
        let install = linux(PathBuf::from("/usr/bin/stelly"), None, UBUNTU_2604);
        assert!(matches!(install, Install::Deb { ubuntu: "26.04", .. }));
    }

    #[test]
    fn a_writable_directory_is_the_tarball() {
        let dir = std::env::temp_dir().join(format!("stelly-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let install = linux(dir.join("stelly"), None, FEDORA);
        assert!(matches!(install, Install::Tarball { ubuntu: "24.04", .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quotes_for_powershell() {
        assert_eq!(powershell_quote(r"C:\Users\O'Neil\x.exe"), r"'C:\Users\O''Neil\x.exe'");
    }
}
