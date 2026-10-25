// whitenoise: a small window app that plays shaped noise through the default audio output.
// Modeled on myNoise's "White Noise & Co" generator: 10 octave bands (32 Hz .. 16 kHz), each a
// band of noise with its own slider. Flat sliders give pink noise (equal power per octave).
#![windows_subsystem = "windows"]

mod hotkey;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontW, HFONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
use windows_sys::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, GetCurrentProcess, GetCurrentThread,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
    SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Controls::{
    ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, TBM_SETPOS, TBM_SETRANGE,
    TBS_BOTH, TBS_NOTICKS, TBS_VERT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BM_SETCHECK, BS_CHECKBOX, BS_PUSHBUTTON, BS_PUSHLIKE, CW_USEDEFAULT,
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetDlgItem, GetMessageW, GetSystemMetrics,
    GetWindowRect, IDC_ARROW, KillTimer, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SendMessageW,
    SetTimer, SetWindowTextW, TranslateMessage, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_EXITSIZEMOVE, WM_SETFONT, WM_TIMER, WM_VSCROLL, WNDCLASSW, WS_CAPTION, WS_CHILD,
    WS_MINIMIZEBOX, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

// ---------------------------------------------------------------------------------------------
// Settings shared between the UI thread and the audio callback (lock-free: f32 bits in atomics).

const BANDS: usize = 10;
const BAND_HZ: [f32; BANDS] = [32., 64., 125., 250., 500., 1000., 2000., 4000., 8000., 16000.];
const BAND_LABELS: [&str; BANDS] = ["32", "64", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];

/// Slider positions 0..1, read off the myNoise "Speech Blocker" preset with the high mids nudged
/// up (from the screenshot).
const PRESET_SPEECH: [f32; BANDS] = [0.21, 0.31, 0.40, 0.56, 0.64, 0.59, 0.49, 0.31, 0.20, 0.09];

/// Output volume in dB (applied on top of the bands). -17 dB puts the default preset at about
/// -20 dBFS RMS, matching the reading in the screenshot.
const DEFAULT_VOLUME_DB: f32 = -17.0;
const VOLUME_MIN_DB: f32 = -60.0;

static BAND_GAIN: [AtomicU32; BANDS] = [const { AtomicU32::new(0) }; BANDS]; // linear amplitude
static MASTER_GAIN: AtomicU32 = AtomicU32::new(0); // linear; 0 while paused
static STREAM_FAILED: AtomicBool = AtomicBool::new(false);

fn store(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Relaxed);
}
fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Relaxed))
}

/// Slider position (0..1) to linear amplitude. Squared, so the slider is roughly dB-like
/// (0.5 = -12 dB) and the bottom is silence.
fn slider_gain(v: f32) -> f32 {
    v * v
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

// ---------------------------------------------------------------------------------------------
// DSP

/// xorshift64* uniform noise in [-1, 1).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

/// RBJ band-pass biquad (0 dB peak), one octave wide.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}
impl Biquad {
    fn bandpass(f0: f32, fs: f32) -> Self {
        let q = std::f32::consts::SQRT_2; // one octave
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Biquad {
            b0: alpha / a0,
            b2: -alpha / a0,
            a1: -2.0 * w0.cos() / a0,
            a2: (1.0 - alpha) / a0,
            ..Default::default()
        }
    }
    fn run(&mut self, x: f32) -> f32 {
        // Transposed direct form II.
        let y = self.b0 * x + self.z1;
        self.z1 = -self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// One channel of noise: an independent noise source per band (so band powers add exactly),
/// each normalized to unit RMS, then scaled by its slider.
struct Generator {
    rng: Rng,
    filters: [Biquad; BANDS],
    norm: [f32; BANDS],
    gain: [f32; BANDS],   // smoothed toward BAND_GAIN
    master: f32,          // smoothed toward MASTER_GAIN
    smooth: f32,          // one-pole coefficient (~30 ms)
}

impl Generator {
    fn new(fs: f32, seed: u64) -> Self {
        let mut rng = Rng(seed | 1);
        let mut filters = [Biquad::default(); BANDS];
        let mut norm = [0.0; BANDS];
        for i in 0..BANDS {
            // Bands at or above Nyquist are left silent (norm 0).
            if BAND_HZ[i] < fs * 0.45 {
                let mut f = Biquad::bandpass(BAND_HZ[i], fs);
                // Measure the band's RMS on a couple of seconds of noise, then normalize.
                let n = (fs * 2.0) as usize;
                let mut sum = 0.0f64;
                for _ in 0..n {
                    let y = f.run(rng.next()) as f64;
                    sum += y * y;
                }
                norm[i] = (n as f64 / sum).sqrt() as f32;
                filters[i] = Biquad::bandpass(BAND_HZ[i], fs);
            }
        }
        Generator {
            rng,
            filters,
            norm,
            gain: [0.0; BANDS],
            master: 0.0,
            smooth: 1.0 - (-1.0 / (0.03 * fs)).exp(),
        }
    }

    fn next(&mut self, target: &[f32; BANDS], master: f32) -> f32 {
        let k = self.smooth;
        self.master += (master - self.master) * k;
        let mut out = 0.0;
        for i in 0..BANDS {
            self.gain[i] += (target[i] - self.gain[i]) * k;
            let x = self.rng.next();
            out += self.filters[i].run(x) * self.norm[i] * self.gain[i];
        }
        (out * self.master).clamp(-1.0, 1.0)
    }
}

// ---------------------------------------------------------------------------------------------
// Audio output

fn build_stream() -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let fmt = config.sample_format();
    let mut config: cpal::StreamConfig = config.into();
    let open = |config: &cpal::StreamConfig| match fmt {
        cpal::SampleFormat::F32 => run_stream::<f32>(&device, config),
        cpal::SampleFormat::I16 => run_stream::<i16>(&device, config),
        cpal::SampleFormat::U16 => run_stream::<u16>(&device, config),
        cpal::SampleFormat::I32 => run_stream::<i32>(&device, config),
        f => Err(format!("unsupported sample format {f}")),
    };
    // A deep buffer (~200 ms) rides out stalls from other activity; latency doesn't matter
    // for noise (slider moves just take effect a fraction of a second later).
    let default = config.clone();
    config.buffer_size = cpal::BufferSize::Fixed(config.sample_rate.0 / 5);
    open(&config).or_else(|_| open(&default))
}

fn run_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
) -> Result<cpal::Stream, String> {
    let fs = config.sample_rate.0 as f32;
    let channels = config.channels as usize;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    // Independent noise per channel: wider, more diffuse than identical mono in both ears.
    let mut gens: Vec<Generator> = (0..channels)
        .map(|c| Generator::new(fs, seed.wrapping_add(0x9E37_79B9_7F4A_7C15u64.wrapping_mul(c as u64 + 1))))
        .collect();

    let mut boosted = false;
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                if !boosted {
                    boosted = true;
                    boost_audio_thread();
                }
                let target: [f32; BANDS] = std::array::from_fn(|i| load(&BAND_GAIN[i]));
                let master = load(&MASTER_GAIN);
                for frame in data.chunks_mut(channels) {
                    for (s, g) in frame.iter_mut().zip(gens.iter_mut()) {
                        *s = T::from_sample(g.next(&target, master));
                    }
                }
            },
            |_err| STREAM_FAILED.store(true, Relaxed),
            None,
        )
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Register the audio thread with MMCSS ("Pro Audio") and raise its priority, so other
/// activity on the machine can't starve it. (cpal 0.15's own boost passes a thread *id* where
/// a handle is required, so it silently does nothing.)
fn boost_audio_thread() {
    unsafe {
        let task = wide("Pro Audio");
        let mut index = 0u32;
        AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

/// Keep the machine from idle-sleeping while playing (the display may still turn off).
fn keep_awake(on: bool) {
    unsafe {
        SetThreadExecutionState(if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
    }
}

/// Opt out of EcoQoS / power throttling, which Windows applies to background and minimized
/// windows and which can slow our threads down.
fn disable_power_throttling() {
    unsafe {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Persisted state: window position, play state, volume, sliders.

struct State {
    pos: Option<(i32, i32)>,
    playing: bool,
    volume_db: f32,
    sliders: [f32; BANDS],
}

impl Default for State {
    fn default() -> Self {
        State { pos: None, playing: true, volume_db: DEFAULT_VOLUME_DB, sliders: PRESET_SPEECH }
    }
}

fn state_file() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("whitenoise").join("state.txt"))
}

/// Format: `x y playing volume_db s1 .. s10`, whitespace separated.
fn load_state() -> State {
    let parse = || -> Option<State> {
        let text = std::fs::read_to_string(state_file()?).ok()?;
        let v: Vec<f32> = text.split_whitespace().map(|t| t.parse().ok()).collect::<Option<_>>()?;
        if v.len() != 4 + BANDS {
            return None;
        }
        Some(State {
            pos: Some((v[0] as i32, v[1] as i32)),
            playing: v[2] != 0.0,
            volume_db: v[3].clamp(VOLUME_MIN_DB, 0.0),
            sliders: std::array::from_fn(|i| v[4 + i].clamp(0.0, 1.0)),
        })
    };
    parse().unwrap_or_default()
}

fn save_state(s: &State) {
    let Some(path) = state_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let (x, y) = s.pos.unwrap_or((0, 0));
    let mut text = format!("{x} {y} {} {:.1}", s.playing as u8, s.volume_db);
    for v in s.sliders {
        text += &format!(" {v:.3}");
    }
    let _ = std::fs::write(path, text);
}

// ---------------------------------------------------------------------------------------------
// Window

const SLIDER_W: i32 = 36;
const SLIDER_H: i32 = 180;
const MARGIN: i32 = 12;
const CLIENT_W: i32 = MARGIN * 2 + SLIDER_W * (BANDS as i32 + 1) + 16;
const CLIENT_H: i32 = 290;

const ID_BAND0: i32 = 100; // .. ID_BAND0 + 9
const ID_VOLUME: i32 = 120;
const ID_PLAY: i32 = 200;
const ID_SPEECH: i32 = 201;
const ID_PINK: i32 = 202;
const ID_WHITE: i32 = 203;
const ID_BROWN: i32 = 204;
const ID_LEVEL: i32 = 300;

const SS_CENTER: u32 = 1;
const TBM_GETPOS: u32 = 0x400; // WM_USER
const TIMER_PAUSE: usize = 1;
const TIMER_WATCHDOG: usize = 2;

thread_local! {
    static APP: RefCell<App> = RefCell::new(App { state: State::default(), stream: None });
}

struct App {
    state: State,
    stream: Option<cpal::Stream>,
}

/// Append a timestamped line to %LOCALAPPDATA%\whitenoise\whitenoise.log (diagnostics only).
pub(crate) fn log(msg: &str) {
    use std::io::Write;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock();
    let Some(path) = state_file().map(|p| p.with_file_name("whitenoise.log")) else { return };
    // Keep it small: start over once it passes 256 KB.
    let append = std::fs::metadata(&path).map(|m| m.len() < 256 * 1024).unwrap_or(false);
    let file = std::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(&path);
    if let Ok(mut f) = file {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{secs} {msg}");
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    unsafe { CreateFontW(height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr()) }
}

#[allow(clippy::too_many_arguments)]
unsafe fn make_child(
    parent: HWND,
    class: *const u16,
    text: &str,
    style: u32,
    (x, y, w, h): (i32, i32, i32, i32),
    id: i32,
    font: HFONT,
) -> HWND {
    let text_w = wide(text);
    unsafe {
        let hwnd = CreateWindowExW(
            0,
            class,
            text_w.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            parent,
            id as usize as _,
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        );
        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
        hwnd
    }
}

/// Vertical trackbars put their minimum at the top; we want 1.0 at the top.
unsafe fn slider_set(hwnd: HWND, id: i32, v: f32) {
    unsafe {
        let pos = 100 - (v * 100.0).round() as isize;
        SendMessageW(GetDlgItem(hwnd, id), TBM_SETPOS, 1, pos);
    }
}
unsafe fn slider_get(hwnd: HWND, id: i32) -> f32 {
    unsafe { (100 - SendMessageW(GetDlgItem(hwnd, id), TBM_GETPOS, 0, 0) as i32) as f32 / 100.0 }
}

fn volume_to_slider(db: f32) -> f32 {
    (db - VOLUME_MIN_DB) / -VOLUME_MIN_DB
}
fn slider_to_volume(v: f32) -> f32 {
    VOLUME_MIN_DB + v * -VOLUME_MIN_DB
}

/// Push the current state into the audio thread's targets and refresh the level/button UI.
unsafe fn apply(hwnd: HWND, app: &App) {
    let s = &app.state;
    let gains: [f32; BANDS] = std::array::from_fn(|i| slider_gain(s.sliders[i]));
    for i in 0..BANDS {
        store(&BAND_GAIN[i], gains[i]);
    }
    let master = if s.playing { db_to_lin(s.volume_db) } else { 0.0 };
    store(&MASTER_GAIN, master);
    keep_awake(s.playing);

    // Bands are independent unit-RMS noise, so total RMS = master * sqrt(sum of gain^2).
    let rms = db_to_lin(s.volume_db) * gains.iter().map(|g| g * g).sum::<f32>().sqrt();
    let level = if rms > 0.0 {
        format!("{:.0} dBFS", 20.0 * rms.log10())
    } else {
        "silent".to_string()
    };
    unsafe {
        let w = wide(&level);
        SetWindowTextW(GetDlgItem(hwnd, ID_LEVEL), w.as_ptr());
        SendMessageW(GetDlgItem(hwnd, ID_PLAY), BM_SETCHECK, s.playing as usize, 0);
        let label = wide(if s.playing { "Pause" } else { "Play" });
        SetWindowTextW(GetDlgItem(hwnd, ID_PLAY), label.as_ptr());
    }
}

unsafe fn set_sliders(hwnd: HWND, app: &mut App, v: [f32; BANDS]) {
    app.state.sliders = v;
    for (i, &x) in v.iter().enumerate() {
        unsafe { slider_set(hwnd, ID_BAND0 + i as i32, x) };
    }
}

fn start_stream(app: &mut App) {
    if app.stream.is_none() {
        STREAM_FAILED.store(false, Relaxed);
        app.stream = build_stream().ok();
    }
    if let Some(s) = &app.stream {
        let _ = s.play();
    }
}

/// Presets in slider units. Pink = flat; white/brown tilt by +/-3 dB per octave
/// (slider gain is squared, so a quarter power of frequency in slider units).
fn preset(id: i32) -> Option<[f32; BANDS]> {
    let tilt = |exp: f32| std::array::from_fn(|i| 0.7 * (BAND_HZ[i] / 1000.0).powf(exp) / 2f32.powf(4.0 * exp.abs()));
    match id {
        ID_SPEECH => Some(PRESET_SPEECH),
        ID_PINK => Some([0.5; BANDS]),
        ID_WHITE => Some(tilt(0.25)),
        ID_BROWN => Some(tilt(-0.25)),
        _ => None,
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let small = make_font(-12, 400);
                let btn = make_font(-15, 500);
                let level = make_font(-18, 500);
                let trackbar = wide("msctls_trackbar32");
                let button = wide("BUTTON");
                let stat = wide("STATIC");

                let top = MARGIN;
                for i in 0..BANDS {
                    let x = MARGIN + SLIDER_W * i as i32;
                    let tb = make_child(
                        hwnd,
                        trackbar.as_ptr(),
                        "",
                        TBS_VERT | TBS_BOTH | TBS_NOTICKS | WS_TABSTOP,
                        (x, top, SLIDER_W, SLIDER_H),
                        ID_BAND0 + i as i32,
                        small,
                    );
                    SendMessageW(tb, TBM_SETRANGE, 1, (100 << 16) as isize);
                    make_child(hwnd, stat.as_ptr(), BAND_LABELS[i], SS_CENTER, (x, top + SLIDER_H, SLIDER_W, 16), -1, small);
                }
                // Volume slider, set apart on the right.
                let vx = MARGIN + SLIDER_W * BANDS as i32 + 16;
                let tb = make_child(
                    hwnd,
                    trackbar.as_ptr(),
                    "",
                    TBS_VERT | TBS_BOTH | TBS_NOTICKS | WS_TABSTOP,
                    (vx, top, SLIDER_W, SLIDER_H),
                    ID_VOLUME,
                    small,
                );
                SendMessageW(tb, TBM_SETRANGE, 1, (100 << 16) as isize);
                make_child(hwnd, stat.as_ptr(), "Vol", SS_CENTER, (vx, top + SLIDER_H, SLIDER_W, 16), -1, small);

                // Buttons and level readout.
                let by = top + SLIDER_H + 28;
                let play_style = WS_TABSTOP | (BS_PUSHLIKE | BS_CHECKBOX) as u32;
                make_child(hwnd, button.as_ptr(), "Pause", play_style, (MARGIN, by, 80, 32), ID_PLAY, btn);
                let names = [(ID_SPEECH, "Speech"), (ID_PINK, "Pink"), (ID_WHITE, "White"), (ID_BROWN, "Brown")];
                for (n, (id, name)) in names.iter().enumerate() {
                    let x = MARGIN + 92 + n as i32 * 66;
                    make_child(hwnd, button.as_ptr(), name, WS_TABSTOP | BS_PUSHBUTTON as u32, (x, by, 62, 32), *id, btn);
                }
                make_child(hwnd, stat.as_ptr(), "", SS_CENTER, (MARGIN, by + 40, CLIENT_W - 2 * MARGIN, 24), ID_LEVEL, level);

                APP.with_borrow_mut(|app| {
                    set_sliders(hwnd, app, app.state.sliders);
                    slider_set(hwnd, ID_VOLUME, volume_to_slider(app.state.volume_db));
                    apply(hwnd, app);
                    if app.state.playing {
                        start_stream(app);
                    }
                });
                SetTimer(hwnd, TIMER_WATCHDOG, 2000, None);
                // M5 (laptop) / star (Natural 4000) act like clicking Play/Pause.
                hotkey::start(hwnd, ID_PLAY);
                0
            }
            WM_VSCROLL => {
                APP.with_borrow_mut(|app| {
                    app.state.sliders = std::array::from_fn(|i| slider_get(hwnd, ID_BAND0 + i as i32));
                    app.state.volume_db = slider_to_volume(slider_get(hwnd, ID_VOLUME));
                    apply(hwnd, app);
                });
                0
            }
            WM_COMMAND => {
                let id = (wparam & 0xFFFF) as i32;
                APP.with_borrow_mut(|app| {
                    if id == ID_PLAY {
                        app.state.playing = !app.state.playing;
                        apply(hwnd, app);
                        if app.state.playing {
                            KillTimer(hwnd, TIMER_PAUSE);
                            start_stream(app);
                        } else {
                            // Let the gain ramp fade out before stopping the device.
                            SetTimer(hwnd, TIMER_PAUSE, 150, None);
                        }
                    } else if let Some(v) = preset(id) {
                        set_sliders(hwnd, app, v);
                        apply(hwnd, app);
                    }
                });
                0
            }
            WM_TIMER => {
                APP.with_borrow_mut(|app| match wparam {
                    TIMER_PAUSE => {
                        KillTimer(hwnd, TIMER_PAUSE);
                        if let Some(s) = &app.stream {
                            let _ = s.pause();
                        }
                    }
                    TIMER_WATCHDOG => {
                        // Device unplugged or changed: rebuild on the new default device.
                        if STREAM_FAILED.load(Relaxed) {
                            app.stream = None;
                            if app.state.playing {
                                start_stream(app);
                            }
                        }
                    }
                    _ => {}
                });
                0
            }
            WM_EXITSIZEMOVE | WM_DESTROY => {
                let mut r: RECT = std::mem::zeroed();
                APP.with_borrow_mut(|app| {
                    if GetWindowRect(hwnd, &mut r) != 0 {
                        app.state.pos = Some((r.left, r.top));
                    }
                    save_state(&app.state);
                    if msg == WM_DESTROY {
                        app.stream = None;
                    }
                });
                if msg == WM_DESTROY {
                    PostQuitMessage(0);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Keep a saved top-left within the virtual desktop (e.g. after a monitor is disconnected).
fn clamp_onscreen(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (x.clamp(vx, (vx + vw - w).max(vx)), y.clamp(vy, (vy + vh - h).max(vy)))
    }
}

fn main() {
    let state = load_state();
    let saved_pos = state.pos;
    APP.with_borrow_mut(|app| app.state = state);

    disable_power_throttling();
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
        };
        InitCommonControlsEx(&icc);

        let hinstance = GetModuleHandleW(ptr::null());
        let class_name = wide("whitenoiseWindow");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: 16 as *mut core::ffi::c_void, // COLOR_BTNFACE + 1
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            return;
        }

        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE;
        let mut r = RECT { left: 0, top: 0, right: CLIENT_W, bottom: CLIENT_H };
        AdjustWindowRect(&mut r, style, 0);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let (x, y) = match saved_pos {
            Some((sx, sy)) => clamp_onscreen(sx, sy, w, h),
            None => (CW_USEDEFAULT, CW_USEDEFAULT),
        };
        let title = wide("White Noise");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            w,
            h,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preset_is_about_minus_20_dbfs() {
        let fs = 48000.0;
        let mut g = Generator::new(fs, 42);
        let target: [f32; BANDS] = std::array::from_fn(|i| slider_gain(PRESET_SPEECH[i]));
        let master = db_to_lin(DEFAULT_VOLUME_DB);
        for _ in 0..(fs as usize) {
            g.next(&target, master); // settle the gain ramps
        }
        let n = 5 * fs as usize;
        let sum: f64 = (0..n).map(|_| (g.next(&target, master) as f64).powi(2)).sum();
        let db = 10.0 * (sum / n as f64).log10();
        assert!((db + 20.0).abs() < 1.0, "got {db:.2} dBFS");
    }
}
