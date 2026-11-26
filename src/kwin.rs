//! "Keep above other windows" via the desktop's native window manager.

#[cfg(not(windows))]
use std::process::Command;
#[cfg(not(windows))]
use std::thread;

use anyhow::Result;
#[cfg(not(windows))]
use anyhow::{Context, bail};

#[cfg(not(windows))]
fn gdbus(path: &str, method: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.KWin",
            "--object-path",
            path,
            "--method",
            method,
        ])
        .args(args)
        .output()
        .context("cannot run gdbus")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(windows))]
fn apply(above: bool) -> Result<()> {
    let pid = std::process::id();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let file = std::path::PathBuf::from(runtime).join(format!("clueless-kwin-{pid}.js"));
    std::fs::write(
        &file,
        format!("for (const w of workspace.windowList()) {{ if (w.pid === {pid}) w.keepAbove = {above}; }}\n"),
    )?;
    let name = format!("clueless-{pid}");
    let scripting = "org.kde.kwin.Scripting";
    // A leftover script with the same name would make loading fail.
    let _ = gdbus("/Scripting", &format!("{scripting}.unloadScript"), &[&name]);
    let reply = gdbus(
        "/Scripting",
        &format!("{scripting}.loadScript"),
        &[&file.to_string_lossy(), &name],
    )?;
    let id: i32 = reply
        .trim_matches(|c: char| !c.is_ascii_digit() && c != '-')
        .parse()
        .with_context(|| format!("unexpected KWin reply {reply}"))?;
    if id < 0 {
        bail!("KWin refused the script");
    }
    let run = gdbus(&format!("/Scripting/Script{id}"), "org.kde.kwin.Script.run", &[]);
    thread::sleep(std::time::Duration::from_millis(300));
    let _ = gdbus("/Scripting", &format!("{scripting}.unloadScript"), &[&name]);
    let _ = std::fs::remove_file(file);
    run.map(|_| ())
}

/// Use the realized GDK Win32 window on the GTK thread. On KDE Plasma a
/// one-shot KWin script runs off that thread.
pub fn apply_in_background(window: &gtk4::ApplicationWindow, above: bool) -> Result<()> {
    #[cfg(windows)]
    return crate::windows::set_topmost(window, above);
    #[cfg(not(windows))]
    {
        let _ = window;
        thread::spawn(move || {
            if let Err(e) = apply(above) {
                eprintln!("clueless: keep-above unavailable ({e}). Use the window menu instead.");
            }
        });
        Ok(())
    }
}
