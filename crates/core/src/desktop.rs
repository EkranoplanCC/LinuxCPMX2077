//! Talking to the Linux desktop: opening links in the user's browser and
//! registering the app for "Mod Manager Download" (`nxm://`) links.
//!
//! Inside an AppImage the launcher points GTK/GIO at the bundled libraries
//! (`GIO_MODULE_DIR`, `GTK_PATH`, an `XDG_DATA_DIRS` entry under `$APPDIR`, ...).
//! A browser started with that environment loads the wrong modules and often
//! dies silently, so every helper program started here gets the user's own
//! environment back first.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// Variables the AppImage launcher (linuxdeploy's GTK hook) and runtime set.
const APPIMAGE_VARS: &[&str] = &[
    "APPDIR",
    "APPIMAGE",
    "ARGV0",
    "OWD",
    "GTK_DATA_PREFIX",
    "GTK_THEME",
    "GTK_EXE_PREFIX",
    "GTK_PATH",
    "GTK_IM_MODULE_FILE",
    "GSETTINGS_SCHEMA_DIR",
    "GI_TYPELIB_PATH",
    "GIO_MODULE_DIR",
    "GDK_PIXBUF_MODULE_FILE",
];

/// Hosts the app may open in the browser.
const BROWSER_HOSTS: &[&str] = &["nexusmods.com", "github.com"];

/// Changes that turn the app's environment back into the user's: `Some` sets
/// a variable, `None` removes it. Outside an AppImage nothing changes.
pub fn clean_env(vars: &[(String, String)]) -> Vec<(String, Option<String>)> {
    let Some(appdir) = vars.iter().find(|(k, _)| k == "APPDIR").map(|(_, v)| v.trim_end_matches('/').to_string()) else {
        return vec![];
    };
    if appdir.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for (k, v) in vars {
        if APPIMAGE_VARS.contains(&k.as_str()) {
            out.push((k.clone(), None));
            continue;
        }
        // Path lists such as XDG_DATA_DIRS or PATH: drop entries inside the
        // AppImage, keep the user's own.
        if v.contains(&appdir) {
            let kept: Vec<&str> =
                v.split(':').filter(|p| !p.is_empty() && !p.starts_with(&appdir) && !p.starts_with(&format!("{appdir}/"))).collect();
            if kept.is_empty() {
                out.push((k.clone(), None));
            } else {
                out.push((k.clone(), Some(kept.join(":"))));
            }
        }
    }
    out
}

/// A command that runs with the user's environment rather than the AppImage's.
pub fn user_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    let vars: Vec<(String, String)> = std::env::vars().collect();
    for (k, v) in clean_env(&vars) {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd
}

/// Only https links to the hosts the app works with.
pub fn is_browser_url(u: &str) -> bool {
    let Ok(url) = url::Url::parse(u) else { return false };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    url.scheme() == "https" && BROWSER_HOSTS.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Open a link in the user's default browser. Waits briefly so a launcher
/// that fails straight away is reported instead of silently doing nothing.
pub fn open_url(url: &str) -> Result<()> {
    if !is_browser_url(url) {
        return Err(Error::Other(format!("refusing to open {url}")));
    }
    launch(url)
}

/// Show a folder in the user's file manager.
pub fn open_folder(dir: &std::path::Path) -> Result<()> {
    if !dir.is_dir() {
        return Err(Error::Other(format!("{} is not a folder", dir.display())));
    }
    launch(&dir.to_string_lossy())
}

/// Open any https link the user clicked on a web page shown in the app
/// (the in-app Nexus window), the way a browser opens a new tab.
pub fn open_link(url: &str) -> Result<()> {
    match url::Url::parse(url) {
        Ok(u) if u.scheme() == "https" && u.host_str().is_some() => launch(u.as_str()),
        _ => Err(Error::Other(format!("refusing to open {url}"))),
    }
}

fn launch(url: &str) -> Result<()> {
    let mut last_err = String::from("no program to open links was found (install xdg-utils)");
    for (program, args) in [("xdg-open", vec![]), ("gio", vec!["open"])] {
        let child = user_command(program)
            .args(&args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                last_err = format!("{program}: {e}");
                continue;
            }
        };
        let started = Instant::now();
        loop {
            match child.try_wait()? {
                Some(status) if status.success() => return Ok(()),
                Some(status) => {
                    let mut err = String::new();
                    if let Some(mut s) = child.stderr.take() {
                        use std::io::Read;
                        let _ = s.read_to_string(&mut err);
                    }
                    let err = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                    last_err = format!("{program} failed ({status}){}", if err.is_empty() { String::new() } else { format!(": {err}") });
                    break;
                }
                // Some launchers stay around while the browser runs: that's a
                // success. Reap it in the background.
                None if started.elapsed() > Duration::from_secs(3) => {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(());
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
    Err(Error::Other(format!("could not open your browser: {last_err}")))
}

const NXM_MIME: &str = "x-scheme-handler/nxm";
/// Our handler's desktop entry, in `~/.local/share/applications`.
pub const NXM_DESKTOP_FILE: &str = "cpmx2077-nxm-handler.desktop";

/// Quote a path for a desktop entry's `Exec` key.
pub fn desktop_exec_quote(path: &str) -> String {
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn nxm_desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=CPMX2077 (Nexus downloads)\nExec={} %u\nTerminal=false\nNoDisplay=true\nMimeType={NXM_MIME};\n",
        desktop_exec_quote(exe)
    )
}

/// The program `nxm://` links should start: the AppImage file itself (its
/// mount point changes every launch), or the binary when not packaged.
pub fn current_launcher() -> Result<String> {
    if let Some(p) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p).to_string_lossy().into_owned());
    }
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

pub fn applications_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("applications")
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NxmStatus {
    /// `nxm://` links open this app.
    pub registered: bool,
    /// The desktop entry currently handling them, if any.
    pub handler: Option<String>,
}

/// Who handles `nxm://` links right now.
pub fn nxm_status() -> NxmStatus {
    let handler = user_command("xdg-mime")
        .args(["query", "default", NXM_MIME])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    NxmStatus { registered: handler.as_deref() == Some(NXM_DESKTOP_FILE), handler }
}

/// Make this app open `nxm://` links. Replaces whichever app handled them.
pub fn register_nxm(home: &Path) -> Result<NxmStatus> {
    let dir = applications_dir(home);
    write_nxm_entry(&dir, &current_launcher()?)?;
    // Optional: refreshes the desktop's cache; not installed everywhere.
    let _ = user_command("update-desktop-database").arg(&dir).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let ok = user_command("xdg-mime")
        .args(["default", NXM_DESKTOP_FILE, NXM_MIME])
        .status()
        .map_err(|e| Error::Other(format!("could not run xdg-mime (install xdg-utils): {e}")))?;
    if !ok.success() {
        return Err(Error::Other(format!("xdg-mime failed ({ok})")));
    }
    Ok(nxm_status())
}

fn write_nxm_entry(dir: &Path, exe: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(NXM_DESKTOP_FILE);
    let entry = nxm_desktop_entry(exe);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str()) {
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut tmp, entry.as_bytes())?;
        tmp.persist(&path).map_err(|e| Error::Io(e.error))?;
    }
    Ok(())
}

/// After an update the AppImage usually lives at a new path. If we already
/// own the handler entry, point it at this copy so links keep working.
pub fn refresh_nxm_entry(home: &Path) -> Result<()> {
    refresh_entry_in(&applications_dir(home), &current_launcher()?)
}

fn refresh_entry_in(dir: &Path, exe: &str) -> Result<()> {
    if dir.join(NXM_DESKTOP_FILE).is_file() {
        write_nxm_entry(dir, exe)?;
    }
    Ok(())
}

pub fn nexus_download_page(mod_id: i64, file_id: i64) -> String {
    // `nmm=1` goes straight to the "Mod Manager Download" page for the file.
    format!("https://www.nexusmods.com/{}/mods/{mod_id}?tab=files&file_id={file_id}&nmm=1", crate::NEXUS_GAME_DOMAIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn strips_appimage_environment() {
        let env = v(&[
            ("APPDIR", "/tmp/.mount_CPMXabc"),
            ("APPIMAGE", "/home/u/CPMX.AppImage"),
            ("GIO_MODULE_DIR", "/tmp/.mount_CPMXabc//usr/lib/gio/modules"),
            ("GTK_THEME", ""),
            ("XDG_DATA_DIRS", "/tmp/.mount_CPMXabc/usr/share:/usr/share:/usr/local/share"),
            ("PATH", "/tmp/.mount_CPMXabc/usr/bin:/usr/bin"),
            ("HOME", "/home/u"),
            ("LANG", "en_US.UTF-8"),
        ]);
        let changes = clean_env(&env);
        let get = |k: &str| changes.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("APPDIR"), Some(None));
        assert_eq!(get("GIO_MODULE_DIR"), Some(None));
        assert_eq!(get("GTK_THEME"), Some(None));
        assert_eq!(get("XDG_DATA_DIRS"), Some(Some("/usr/share:/usr/local/share".into())));
        assert_eq!(get("PATH"), Some(Some("/usr/bin".into())));
        assert_eq!(get("HOME"), None, "untouched");
        assert!(clean_env(&v(&[("PATH", "/usr/bin")])).is_empty(), "not an AppImage: nothing to do");
    }

    #[test]
    fn only_opens_known_https_hosts() {
        assert!(is_browser_url("https://www.nexusmods.com/cyberpunk2077/mods/107?tab=files&file_id=1&nmm=1"));
        assert!(is_browser_url("https://github.com/psiberx/cp2077-archive-xl/releases"));
        assert!(!is_browser_url("http://www.nexusmods.com/"));
        assert!(!is_browser_url("https://nexusmods.com.evil.example/"));
        assert!(!is_browser_url("file:///etc/passwd"));
        assert!(open_url("https://example.com/").is_err());
    }

    #[test]
    fn desktop_entry_quotes_paths() {
        assert_eq!(desktop_exec_quote("/home/u/My Apps/CPMX 100%.AppImage"), "\"/home/u/My Apps/CPMX 100%%.AppImage\"");
        assert_eq!(desktop_exec_quote("/a/$b\"c"), "\"/a/\\$b\\\"c\"");
        let e = nxm_desktop_entry("/opt/CPMX.AppImage");
        assert!(e.contains("Exec=\"/opt/CPMX.AppImage\" %u\n"));
        assert!(e.contains("MimeType=x-scheme-handler/nxm;\n"));
    }

    #[test]
    fn refresh_only_touches_our_own_entry() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join(NXM_DESKTOP_FILE);
        refresh_entry_in(dir.path(), "/new/CPMX.AppImage").unwrap();
        assert!(!entry.exists(), "never registers on its own");
        write_nxm_entry(dir.path(), "/old/CPMX.AppImage").unwrap();
        refresh_entry_in(dir.path(), "/new/CPMX.AppImage").unwrap();
        let now = std::fs::read_to_string(&entry).unwrap();
        assert!(now.contains("Exec=\"/new/CPMX.AppImage\" %u"), "{now}");
    }
}
