//! Screenshots of the primary screen.
//!
//! On Windows, use native Win32 capture. On KDE Plasma,
//! Spectacle may, so it captures the whole desktop and the primary output is
//! cut out of it, using the layout `kscreen-doctor` reports. Elsewhere `grim`
//! captures everything.

use anyhow::{Context, Result};
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk4 as gtk;

use crate::files::{self, Attachment};

/// Longer images are scaled down by Claude anyway; sending less is faster.
const MAX_EDGE: i32 = 1920;

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(not(windows))]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The primary output and the bounding box of all enabled outputs, in
/// logical coordinates, from `kscreen-doctor -j`.
#[cfg(not(windows))]
pub fn layout(json: &serde_json::Value) -> Option<(Rect, Rect)> {
    let outputs: Vec<(u64, Rect)> = json["outputs"]
        .as_array()?
        .iter()
        .filter(|o| o["enabled"].as_bool() == Some(true))
        .filter_map(|o| {
            let scale = o["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
            // Rotated outputs report their mode size unrotated.
            let turned = matches!(o["rotation"].as_u64(), Some(2 | 8));
            let (w, h) = (o["size"]["width"].as_f64()?, o["size"]["height"].as_f64()?);
            let (w, h) = if turned { (h, w) } else { (w, h) };
            let rect = Rect {
                x: o["pos"]["x"].as_f64()?,
                y: o["pos"]["y"].as_f64()?,
                width: w / scale,
                height: h / scale,
            };
            // Priority 1 is the primary output; 0 means unset.
            let priority = o["priority"].as_u64().filter(|&p| p > 0).unwrap_or(u64::MAX);
            Some((priority, rect))
        })
        .collect();
    let primary = outputs.iter().min_by_key(|o| o.0)?.1;
    let left = outputs.iter().map(|o| o.1.x).fold(f64::MAX, f64::min);
    let top = outputs.iter().map(|o| o.1.y).fold(f64::MAX, f64::min);
    let right = outputs.iter().map(|o| o.1.x + o.1.width).fold(f64::MIN, f64::max);
    let bottom = outputs.iter().map(|o| o.1.y + o.1.height).fold(f64::MIN, f64::max);
    let all = Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    };
    Some((primary, all))
}

/// Where `primary` lies in an image of the whole desktop `all` that is
/// `width` × `height` pixels: (x, y, width, height).
#[cfg(not(windows))]
pub fn crop(primary: Rect, all: Rect, width: i32, height: i32) -> (i32, i32, i32, i32) {
    let sx = width as f64 / all.width;
    let sy = height as f64 / all.height;
    let x = (((primary.x - all.x) * sx).round() as i32).clamp(0, width - 1);
    let y = (((primary.y - all.y) * sy).round() as i32).clamp(0, height - 1);
    let w = ((primary.width * sx).round() as i32).clamp(1, width - x);
    let h = ((primary.height * sy).round() as i32).clamp(1, height - y);
    (x, y, w, h)
}

#[cfg(not(windows))]
fn run(program: &str, args: &[&str], file: &std::path::Path) -> Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .arg(file)
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    if !out.status.success() || !file.exists() {
        anyhow::bail!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(not(windows))]
fn primary_layout() -> Option<(Rect, Rect)> {
    let out = std::process::Command::new("kscreen-doctor").arg("-j").output().ok()?;
    layout(&serde_json::from_slice(&out.stdout).ok()?)
}

#[cfg(not(windows))]
fn capture_pixels() -> Result<Pixbuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file = runtime.join(format!("clueless-screen-{}.png", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let taken = run("spectacle", &["-b", "-n", "-f", "-o"], &file)
        .map(|_| primary_layout())
        .or_else(|e| run("grim", &[], &file).map(|_| None).map_err(|_| e));
    let image = taken.and_then(|layout| {
        let full = Pixbuf::from_file(&file).context("cannot read the screenshot")?;
        Ok(match layout {
            Some((primary, all)) => {
                let (x, y, w, h) = crop(primary, all, full.width(), full.height());
                full.new_subpixbuf(x, y, w, h)
            }
            None => full,
        })
    });
    let _ = std::fs::remove_file(&file);
    image.context("cannot take a screenshot (needs Spectacle on KDE Plasma, or grim)")
}

/// Captures the primary screen as a JPEG attachment.
pub fn capture() -> Result<Attachment> {
    #[cfg(windows)]
    let image = crate::windows::screen_pixels()?;
    #[cfg(not(windows))]
    let image = capture_pixels()?;
    let (w, h) = (image.width(), image.height());
    let scale = (MAX_EDGE as f64 / w.max(h) as f64).min(1.0);
    let image = if scale < 1.0 {
        let (nw, nh) = ((w as f64 * scale).round() as i32, (h as f64 * scale).round() as i32);
        image
            .scale_simple(nw, nh, InterpType::Bilinear)
            .context("cannot scale the screenshot")?
    } else {
        image
    };
    let jpeg = image.save_to_bufferv("jpeg", &[("quality", "88")])?;
    let time = gtk::glib::DateTime::now_local()
        .and_then(|d| d.format("%H-%M-%S"))
        .map(String::from)
        .unwrap_or_default();
    Ok(files::image_attachment(
        &format!("screen-{time}.jpg"),
        "image/jpeg",
        &jpeg,
    ))
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cuts_out_the_primary_output() {
        let j = json!({"outputs": [
            {"name": "HDMI-A-1", "enabled": true, "priority": 2, "pos": {"x": 0, "y": 0},
             "size": {"width": 1920, "height": 1080}, "scale": 1},
            {"name": "eDP-1", "enabled": true, "priority": 1, "pos": {"x": 0, "y": 1080},
             "size": {"width": 1920, "height": 1080}, "scale": 1},
            {"name": "DP-2", "enabled": false, "priority": 0, "pos": {"x": 5000, "y": 0},
             "size": {"width": 800, "height": 600}, "scale": 1},
        ]});
        let (primary, all) = layout(&j).unwrap();
        assert_eq!(crop(primary, all, 1920, 2160), (0, 1080, 1920, 1080));
        // A desktop captured at twice the logical size.
        assert_eq!(crop(primary, all, 3840, 4320), (0, 2160, 3840, 2160));
    }

    #[test]
    fn scaled_outputs_use_logical_sizes() {
        let j = json!({"outputs": [
            {"enabled": true, "priority": 1, "pos": {"x": 1280, "y": 0},
             "size": {"width": 2560, "height": 1440}, "scale": 2},
            {"enabled": true, "priority": 2, "pos": {"x": 0, "y": 0},
             "size": {"width": 1280, "height": 1024}, "scale": 1},
        ]});
        let (primary, all) = layout(&j).unwrap();
        assert_eq!(primary.width, 1280.0);
        assert_eq!(all.width, 2560.0);
        assert_eq!(crop(primary, all, 2560, 1024), (1280, 0, 1280, 720));
    }
}

#[cfg(test)]
mod live {
    #[test]
    #[ignore = "takes a real screenshot"]
    fn captures_the_primary_screen() {
        let a = super::capture().unwrap();
        let data = a.blocks[1]["source"]["data"].as_str().unwrap();
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        std::fs::write(std::env::var("SHOT").unwrap(), bytes).unwrap();
    }
}
