//! Global play/pause key, read straight from the HID devices.
//!
//! Raw input (WM_INPUT) stops arriving once the secure login screen is up after a lock, so
//! instead we open the keyboards' HID collections and read their input reports on background
//! threads. That works regardless of focus, lock state or which desktop has input.
//!
//! Keys (found with examples/keyprobe.rs):
//! - ASUS laptop "M5" key: vendor collection ff31:0076 (VID 0B05), report `5a 38 ..`;
//!   release is `5a 00 ..`.
//! - Microsoft Natural Ergonomic 4000 "star" key: consumer collection 000c:0001 (VID 045E),
//!   report id 1 with usage 0x0182; release is usage 0.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;
use std::{ptr, thread};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RID_DEVICE_INFO,
    RIDI_DEVICEINFO, RIDI_DEVICENAME, RIM_TYPEHID,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_COMMAND};

#[derive(Clone, Copy)]
enum Kind {
    AsusM5,
    MsStar,
}

/// Device paths that currently have a reader thread.
static OPEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Start watching for the keys; each press posts WM_COMMAND(`command_id`) to `hwnd`.
/// Rescans every few seconds so keyboards plugged in later (or after resume) get picked up.
pub fn start(hwnd: HWND, command_id: i32) {
    let hwnd = hwnd as isize;
    thread::spawn(move || loop {
        for (path, kind) in find_devices() {
            let mut open = OPEN.lock().unwrap();
            let set = open.get_or_insert_with(HashSet::new);
            if set.insert(path.clone()) {
                thread::spawn(move || {
                    read_device(&path, kind, hwnd, command_id);
                    OPEN.lock().unwrap().as_mut().map(|s| s.remove(&path));
                });
            }
        }
        thread::sleep(Duration::from_secs(5));
    });
}

fn find_devices() -> Vec<(String, Kind)> {
    let mut out = Vec::new();
    unsafe {
        let es = size_of::<RAWINPUTDEVICELIST>() as u32;
        let mut n = 0u32;
        GetRawInputDeviceList(ptr::null_mut(), &mut n, es);
        let mut list = vec![std::mem::zeroed::<RAWINPUTDEVICELIST>(); n as usize];
        let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut n, es);
        if got == u32::MAX {
            return out;
        }
        for d in &list[..got as usize] {
            if d.dwType != RIM_TYPEHID {
                continue;
            }
            let mut info: RID_DEVICE_INFO = std::mem::zeroed();
            info.cbSize = size_of::<RID_DEVICE_INFO>() as u32;
            let mut sz = info.cbSize;
            if (GetRawInputDeviceInfoW(d.hDevice, RIDI_DEVICEINFO, (&raw mut info).cast(), &mut sz) as i32) <= 0 {
                continue;
            }
            let h = info.Anonymous.hid;
            let kind = match (h.dwVendorId, h.usUsagePage, h.usUsage) {
                (0x0B05, 0xFF31, 0x0076) => Kind::AsusM5,
                (0x045E, 0x000C, 0x0001) => Kind::MsStar,
                _ => continue,
            };
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let r = GetRawInputDeviceInfoW(d.hDevice, RIDI_DEVICENAME, name.as_mut_ptr().cast(), &mut len);
            if (r as i32) > 1 {
                out.push((String::from_utf16_lossy(&name[..r as usize - 1]), kind));
            }
        }
    }
    out
}

/// Blocking read loop for one HID collection; returns when the device goes away.
fn read_device(path: &str, kind: Kind, hwnd: isize, command_id: i32) {
    let wpath: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let h = CreateFileW(
            wpath.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            crate::log(&format!("hotkey: can't open {path}: {}", std::io::Error::last_os_error()));
            thread::sleep(Duration::from_secs(30)); // don't retry a refused device too eagerly
            return;
        }
        crate::log(&format!("hotkey: watching {path}"));
        let mut buf = [0u8; 256];
        let mut was_down = false;
        loop {
            let mut n = 0u32;
            if ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, ptr::null_mut()) == 0 {
                crate::log(&format!("hotkey: lost {path}: {}", std::io::Error::last_os_error()));
                break;
            }
            let r = &buf[..n as usize];
            let down = match kind {
                Kind::AsusM5 => r.len() > 1 && r[0] == 0x5A && r[1] == 0x38,
                Kind::MsStar => r.len() > 2 && r[0] == 0x01 && u16::from_le_bytes([r[1], r[2]]) == 0x0182,
            };
            // Each device sends other reports too, so act on the press edge only.
            if down && !was_down {
                crate::log("hotkey: toggle");
                PostMessageW(hwnd as HWND, WM_COMMAND, command_id as usize, 0);
            }
            was_down = down;
        }
        CloseHandle(h);
    }
}
