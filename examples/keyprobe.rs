// Diagnostic: log raw HID reports and special (non-typing) keys from every device, plus
// session lock/unlock, to keyprobe.log. Runs for 5 minutes, no visible window.
#![windows_subsystem = "windows"]

use std::io::Write;
use std::ptr;
use std::time::Instant;
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification};
use windows_sys::Win32::UI::Input::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static mut LOG: Option<std::fs::File> = None;
static mut START: Option<Instant> = None;

fn log(s: &str) {
    unsafe {
        let t = (*(&raw const START)).unwrap().elapsed().as_secs_f32();
        if let Some(f) = (*(&raw mut LOG)).as_mut() {
            let _ = writeln!(f, "{t:8.2} {s}");
            let _ = f.flush();
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn device_name(h: HANDLE) -> String {
    unsafe {
        let mut buf = [0u16; 512];
        let mut n = buf.len() as u32;
        let r = GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, buf.as_mut_ptr().cast(), &mut n);
        if (r as i32) <= 0 { return "?".into(); }
        let s = String::from_utf16_lossy(&buf[..(r as usize).saturating_sub(1)]);
        // Keep just the VID/PID/collection part.
        s.split('#').nth(1).unwrap_or(&s).to_string()
    }
}

/// Name of the desktop receiving input ("Default" normally; "Winlogon" or "<secure>" when the
/// secure login screen is up -- a normal process can't open that one).
fn input_desktop() -> String {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME};
    unsafe {
        let d = OpenInputDesktop(0, 0, 0x0001);
        if d.is_null() { return "<secure>".into(); }
        let mut buf = [0u16; 64];
        let mut n = 0u32;
        GetUserObjectInformationW(d as _, UOI_NAME, buf.as_mut_ptr().cast(), 128, &mut n);
        CloseDesktop(d);
        String::from_utf16_lossy(&buf[..(n as usize / 2).saturating_sub(1)])
    }
}

unsafe fn hid_info(h: HANDLE) -> Option<RID_DEVICE_INFO_HID> {
    unsafe {
        let mut info: RID_DEVICE_INFO = std::mem::zeroed();
        info.cbSize = size_of::<RID_DEVICE_INFO>() as u32;
        let mut n = info.cbSize;
        let r = GetRawInputDeviceInfoW(h, RIDI_DEVICEINFO, (&mut info as *mut RID_DEVICE_INFO).cast(), &mut n);
        if (r as i32) <= 0 || info.dwType != RIM_TYPEHID { return None; }
        Some(info.Anonymous.hid)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_INPUT => {
                let mut size = 0u32;
                let hs = size_of::<RAWINPUTHEADER>() as u32;
                GetRawInputData(lparam as _, RID_INPUT, ptr::null_mut(), &mut size, hs);
                let mut buf = vec![0u64; (size as usize).div_ceil(8)];
                if GetRawInputData(lparam as _, RID_INPUT, buf.as_mut_ptr().cast(), &mut size, hs) == size {
                    let raw = &*(buf.as_ptr() as *const RAWINPUT);
                    let bg = if wparam == 1 { "bg" } else { "fg" };
                    match raw.header.dwType {
                        RIM_TYPEKEYBOARD => {
                            let k = raw.data.keyboard;
                            // Only special keys: media/browser/launch VKs, unmapped keys, and
                            // scan codes beyond the normal keyboard. Ordinary typing is not logged.
                            let special = (0xA6..=0xB7).contains(&k.VKey) || k.VKey == 0xFF || k.VKey == 0
                                || (0x7C..=0x87).contains(&k.VKey) || k.MakeCode > 0x60;
                            if special {
                                log(&format!("{bg} desk={} KBD vk={:#04x} make={:#04x} flags={:#x} msg={:#x} dev={}",
                                    input_desktop(), k.VKey, k.MakeCode, k.Flags, k.Message, device_name(raw.header.hDevice)));
                            }
                        }
                        RIM_TYPEHID => {
                            let h = raw.data.hid;
                            let bytes = std::slice::from_raw_parts(h.bRawData.as_ptr(), (h.dwSizeHid * h.dwCount) as usize);
                            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                            let info = hid_info(raw.header.hDevice);
                            let page = info.map(|i| format!("{:04x}:{:04x}", i.usUsagePage, i.usUsage)).unwrap_or_default();
                            let what = match info.map(|i| (i.usUsagePage, i.usUsage)) {
                                Some((0xff31, 0x76)) if bytes.len() > 1 && bytes[1] != 0 => format!("ASUS key {:#04x}", bytes[1]),
                                Some((0xff31, 0x76)) => String::from("ASUS release"),
                                Some((0xff31, _)) => return DefWindowProcW(hwnd, msg, wparam, lparam), // status noise
                                Some((0x0c, 1)) if bytes.len() > 2 => format!("consumer usage {:#06x}", u16::from_le_bytes([bytes[1], bytes[2]])),
                                _ => String::new(),
                            };
                            log(&format!("{bg} desk={} {what:22} {page} [{}] dev={}", input_desktop(), hex.join(" "), device_name(raw.header.hDevice)));
                        }
                        _ => {}
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_WTSSESSION_CHANGE => {
                log(&format!("SESSION change {}", match wparam as u32 { 7 => "LOCK", 8 => "UNLOCK", x => return { log(&format!("SESSION {x}")); 0 } }));
                0
            }
            WM_TIMER => { PostQuitMessage(0); 0 }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn main() {
    unsafe {
        START = Some(Instant::now());
        LOG = std::fs::File::create(std::env::args().nth(1).unwrap_or("keyprobe.log".into())).ok();
        let hinst = GetModuleHandleW(ptr::null());
        let cls = wide("keyprobe");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: cls.as_ptr(), ..std::mem::zeroed() };
        RegisterClassW(&wc);
        // A real (hidden) top-level window, so session notifications arrive.
        let hwnd = CreateWindowExW(0, cls.as_ptr(), cls.as_ptr(), 0, 0, 0, 0, 0, ptr::null_mut(), ptr::null_mut(), hinst, ptr::null());
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);

        // Register every HID top-level collection present, except pointers/touch/sensors.
        let mut n = 0u32;
        let es = size_of::<RAWINPUTDEVICELIST>() as u32;
        GetRawInputDeviceList(ptr::null_mut(), &mut n, es);
        let mut list = vec![std::mem::zeroed::<RAWINPUTDEVICELIST>(); n as usize];
        GetRawInputDeviceList(list.as_mut_ptr(), &mut n, es);
        let mut pairs: Vec<(u16, u16)> = vec![(1, 6)]; // keyboards
        for d in &list[..n as usize] {
            if d.dwType != RIM_TYPEHID { continue; }
            if let Some(i) = hid_info(d.hDevice) {
                log(&format!("device {:04x}:{:04x} vid={:04x} pid={:04x} {}", i.usUsagePage, i.usUsage, i.dwVendorId, i.dwProductId, device_name(d.hDevice)));
                let skip = matches!((i.usUsagePage, i.usUsage), (1, 1) | (1, 2) | (1, 6)) || i.usUsagePage == 0x0D || i.usUsagePage == 0x20;
                if !skip && !pairs.contains(&(i.usUsagePage, i.usUsage)) {
                    pairs.push((i.usUsagePage, i.usUsage));
                }
            }
        }
        let rids: Vec<RAWINPUTDEVICE> = pairs.iter().map(|&(p, u)| RAWINPUTDEVICE { usUsagePage: p, usUsage: u, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd }).collect();
        let ok = RegisterRawInputDevices(rids.as_ptr(), rids.len() as u32, size_of::<RAWINPUTDEVICE>() as u32);
        log(&format!("registered {pairs:04x?} ok={ok}"));

        SetTimer(hwnd, 1, 300_000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
        log("done");
    }
}
