# whitenoise

A small Windows window app that plays shaped noise all night, modeled on myNoise's
"White Noise & Co" generator. It has 10 octave-band sliders (32 Hz to 16 kHz) plus a volume slider.
Flat sliders give pink noise. The default curve is myNoise's "Speech Blocker" preset with the
high mids nudged up.

## Build / run

```powershell
cargo build --release
```

The binary is `target\release\whitenoise.exe` (here `CARGO_TARGET_DIR` puts it under
`%TEMP%\target\release\`). Launch it, and close the window in the morning.

The desktop shortcut "White Noise" points to an installed copy at
`%LOCALAPPDATA%\whitenoise\whitenoise.exe`. After a rebuild, close the app and refresh that copy:

```powershell
Copy-Item "$env:TEMP\target\release\whitenoise.exe" "$env:LOCALAPPDATA\whitenoise\whitenoise.exe" -Force
```

- **Presets:** Speech (the default curve), Pink (flat), White (+3 dB/octave), Brown (-3 dB/octave).
- The readout shows the estimated output level in dBFS RMS. The default is -20 dBFS, like the
  screenshot.
- Sliders, volume, play state and window position are saved to `%LOCALAPPDATA%\whitenoise\state.txt`.

## Play/pause key

- **Keys:** the laptop's **M5** key (ASUS ROG) and the **star** key on a Microsoft Natural
  Ergonomic Keyboard 4000 both toggle play/pause. They work when the window isn't focused, and
  even on the lock screen with the login box showing.
- **How:** `src/hotkey.rs` opens those HID collections directly and reads their reports on
  background threads. Normal key events stop reaching apps once the login box is up, so this
  avoids them.
- **Device changes:** it rescans every 5 s, so a keyboard plugged in later is picked up.
- **Other keys:** `examples/keyprobe.rs` logs the reports special keys send, for finding codes
  for other keys.
- **Log:** `%LOCALAPPDATA%\whitenoise\whitenoise.log` records the devices opened and each toggle.
- **Side effect:** M5 also keeps whatever it does normally, such as opening Armoury Crate if
  that's enabled.

## Built for running overnight

- **Audio thread:** registered with MMCSS ("Pro Audio") at time-critical priority. cpal 0.15's
  own boost is a no-op because it passes a thread id where a handle is needed.
- **Buffer:** ~200 ms of output buffer, so short stalls elsewhere on the machine don't cause a
  dropout.
- **Callback:** no locks and no allocation. The UI passes settings through atomics, and gain
  changes are smoothed so sliders don't click.
- **Sleep:** `SetThreadExecutionState` keeps the PC from idle-sleeping while playing. The
  display can still turn off.
- **Throttling:** the process opts out of EcoQoS power throttling, which Windows otherwise
  applies to minimized windows.
- **Device errors:** if the stream reports an error (e.g. a device is unplugged), it is rebuilt
  on the current default device within 2 s.
- Each stereo channel gets its own noise, which sounds more diffuse than identical mono.

## Not handled

- If another app takes the audio device in exclusive mode, or Windows ducks other sounds during
  a call, the noise will be affected.
