//! Windows SDK types stay behind the C boundary, while Rust owns each resource.

use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::slice;

use anyhow::{Context, Result, bail};
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::glib::{Bytes, translate::ToGlibPtr};
use gtk::prelude::*;
use gtk4 as gtk;

#[repr(C)]
struct Screen {
    pixels: *mut u8,
    width: i32,
    height: i32,
}

unsafe extern "C" {
    fn clueless_default_output(id: *mut *mut u16, length: *mut usize) -> i32;
    fn clueless_free_device_id(id: *mut u16);
    fn clueless_capture_open(mic: i32, sample_rate: u32, capture: *mut *mut c_void) -> i32;
    fn clueless_capture_wait(capture: *mut c_void, milliseconds: u32) -> i32;
    fn clueless_capture_read(
        capture: *mut c_void, data: *mut *mut u8, frames: *mut u32, silent: *mut i32,
    ) -> i32;
    fn clueless_capture_release(capture: *mut c_void, frames: u32) -> i32;
    fn clueless_capture_close(capture: *mut c_void);
    fn clueless_screen_capture(screen: *mut Screen) -> i32;
    fn clueless_screen_free(pixels: *mut u8);
    fn clueless_set_topmost(hwnd: *mut c_void, above: i32) -> i32;
    // GTK exports this on its Win32 GDK backend. Its return value belongs to the surface.
    fn gdk_win32_surface_get_handle(surface: *mut gtk::gdk::ffi::GdkSurface) -> *mut c_void;
}

fn check(result: i32) -> Result<()> {
    if result < 0 {
        let system_error = if result as u32 & 0xffff_0000 == 0x8007_0000 {
            format!(": {}", std::io::Error::from_raw_os_error(result & 0xffff))
        } else {
            String::new()
        };
        bail!("Windows error 0x{:08X}{system_error}", result as u32);
    }
    Ok(())
}

pub fn default_output() -> Option<String> {
    let mut id = null_mut();
    let mut length = 0;
    // The SDK allocates this UTF-16 device ID until it is freed with CoTaskMemFree.
    unsafe {
        check(clueless_default_output(&mut id, &mut length)).ok()?;
        if id.is_null() {
            return None;
        }
        let name = String::from_utf16_lossy(slice::from_raw_parts(id, length));
        clueless_free_device_id(id);
        Some(name)
    }
}

/// Lives on its capture thread so COM initialization and release stay on that thread.
pub struct Wasapi(NonNull<c_void>);

impl Wasapi {
    pub fn start(mic: bool, sample_rate: u32) -> Result<Self> {
        let mut capture = null_mut();
        // On success the bridge transfers one live capture instance to this thread.
        check(unsafe { clueless_capture_open(i32::from(mic), sample_rate, &mut capture) })?;
        Ok(Self(NonNull::new(capture).context("WASAPI returned no capture instance")?))
    }

    pub fn wait(&self) -> Result<()> {
        // A timeout also succeeds. Checking the packet queue then detects a removed device.
        check(unsafe { clueless_capture_wait(self.0.as_ptr(), 100) })
    }

    pub fn append_packet(&mut self, pending: &mut Vec<u8>) -> Result<bool> {
        let mut data = null_mut();
        let mut frames = 0;
        let mut silent = 0;
        // WASAPI owns the packet until ReleaseBuffer. This stream was opened as 16-bit mono.
        unsafe {
            check(clueless_capture_read(self.0.as_ptr(), &mut data, &mut frames, &mut silent))?;
            if frames == 0 {
                return Ok(false);
            }
            let len = frames as usize * 2;
            if silent != 0 {
                pending.resize(pending.len() + len, 0);
            } else if data.is_null() {
                check(clueless_capture_release(self.0.as_ptr(), frames))?;
                bail!("WASAPI returned a packet without audio data");
            } else {
                pending.extend_from_slice(slice::from_raw_parts(data, len));
            }
            check(clueless_capture_release(self.0.as_ptr(), frames))?;
        }
        Ok(true)
    }
}

impl Drop for Wasapi {
    fn drop(&mut self) {
        // This instance owns the bridge capture and has never left its worker thread.
        unsafe { clueless_capture_close(self.0.as_ptr()) };
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // The native bridge allocated this pixel buffer, or left it null on failure.
        unsafe { clueless_screen_free(self.pixels) };
    }
}

pub fn screen_pixels() -> Result<Pixbuf> {
    let mut screen = Screen { pixels: null_mut(), width: 0, height: 0 };
    // The bridge transfers a packed, top-down RGB buffer on success.
    check(unsafe { clueless_screen_capture(&mut screen) }).context("cannot capture the primary monitor")?;
    if screen.pixels.is_null() || screen.width <= 0 || screen.height <= 0 {
        bail!("the primary monitor returned no pixels");
    }
    let stride = screen.width.checked_mul(3).context("screen image is too wide")?;
    let len = (stride as usize).checked_mul(screen.height as usize).context("screen image is too large")?;
    // The buffer remains alive until Screen::drop after its contents are copied.
    let pixels = unsafe { slice::from_raw_parts(screen.pixels, len) }.to_vec();
    let pixels = Bytes::from_owned(pixels);
    Ok(Pixbuf::from_bytes(&pixels, Colorspace::Rgb, false, 8, screen.width, screen.height, stride))
}

pub fn set_topmost(window: &gtk::ApplicationWindow, above: bool) -> Result<()> {
    let surface = window.surface().context("the window has no realized GTK surface")?;
    // The Win32-only GDK surface is alive on the GTK thread for this entire call.
    unsafe {
        let hwnd = gdk_win32_surface_get_handle(surface.to_glib_none().0);
        if hwnd.is_null() {
            bail!("the GTK surface has no native window");
        }
        check(clueless_set_topmost(hwnd, i32::from(above))).context("cannot update On top")
    }
}
