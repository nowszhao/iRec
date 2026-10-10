# iRec

A menu-bar screen recorder for macOS, written in Rust. Full-screen or region capture
with optional system audio, encoded as HEVC + AAC in an `.mp4` container and saved to
the desktop by default.

[简体中文](./README.md) | English

## Contents

- [Features](#features)
- [Requirements](#requirements)
- [Install & Build](#install--build)
- [Usage](#usage)
  - [Control window](#control-window)
  - [Global hotkeys](#global-hotkeys)
  - [Region recording](#region-recording)
  - [Menu bar & settings](#menu-bar--settings)
- [Output & measured size](#output--measured-size)
- [Permissions & troubleshooting](#permissions--troubleshooting)
- [Headless self-test](#headless-self-test)
- [Architecture notes](#architecture-notes)
- [Reliability](#reliability)
- [Known limitations](#known-limitations)
- [App icon](#app-icon)

## Features

| Capability | Details |
| --- | --- |
| One-key capture | `⇧⌘R` starts/stops full-screen recording; `⇧⌘E` opens the region picker and stops an ongoing recording |
| Full screen / region | Region is drag-selected; press any key while selecting to cancel |
| Persistent selection frame | The frame stays on screen after selection, can be dragged and resized, and hosts its own start/stop control bar |
| Quiet menu-bar app | On launch it only appears in the menu bar (no Dock icon, no window); every action lives in the status item menu |
| State-aware controls | Stop is greyed out while idle; start/region are greyed out while recording |
| Never recorded | The control window and the selection frame use `NSWindowSharingType::None`, so they never appear in the capture |
| Audio | Four modes: off / system audio / microphone / system + microphone (mixed into one track) |
| Encoding | HEVC hardware encoding, YUV420 sampling, adjustable quality / frame rate / resolution |
| Output | MP4 (HEVC + AAC) named `iRec-YYYYMMDD-HHMMSS.mp4`, saved to `~/Desktop` |

## Requirements

- macOS 13.0 or later (`LSMinimumSystemVersion = 13.0`)
- Screen Recording permission (requested on first real capture — see [Permissions](#permissions--troubleshooting))
- To build: a Rust toolchain (edition 2024) and Xcode command line tools

## Download

Grab the package that matches your Mac from
[Releases](https://github.com/nowszhao/iRec/releases):

| Mac | Package |
| --- | --- |
| Apple Silicon (M1 / M2 / M3 / M4 …) | `iRec-<version>-arm64.zip` |
| Intel | `iRec-<version>-x86_64.zip` |

If unsure, run `uname -m`: `arm64` means Apple Silicon, `x86_64` means Intel.
Unzip and drag `iRec.app` into Applications.

The bundles are ad-hoc signed (not notarised), so Gatekeeper may block the first
launch: right-click → Open, or run
`xattr -dr com.apple.quarantine /Applications/iRec.app`.

## Install & Build

```sh
git clone ssh://git@ssh.github.com:443/nowszhao/iRec.git
cd iRec

./scripts/build_app.sh              # release build packaged as target/release/iRec.app
cargo build --release               # binary only (target/release/iRec)
```

The script signs ad-hoc by default — **every rebuild produces a new code identity, so macOS
treats it as a new app and the recording permission has to be granted again**. Signing with a
stable developer certificate (fixed TeamIdentifier) avoids that; grant it once after install:

```sh
SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" ./scripts/build_app.sh
```

Install into Applications:

```sh
cp -R target/release/iRec.app /Applications/
```

Build for a specific architecture (default: the current machine):

```sh
./scripts/build_app.sh                 # host architecture
ARCH=arm64 ./scripts/build_app.sh      # Apple Silicon
ARCH=x86_64 ./scripts/build_app.sh     # Intel (cross-compiled)
ARCH=universal ./scripts/build_app.sh  # universal binary (lipo)
```

Build the release packages (one zip per architecture + SHA256SUMS into `dist/`):

```sh
./scripts/package_release.sh
```

Publishing: pushing a `v*` tag triggers GitHub Actions, which builds both
architectures and creates the release.

```sh
git tag v0.1.0 && git push origin v0.1.0
```

## Usage

iRec is a menu-bar app: on launch it stays **quiet** — no Dock icon and no window.
Click the menu bar item (hollow dot + `REC`; it turns red and shows `● 00:12` while
recording, then briefly `✓ 12.3 MB` after finishing) to open the menu, which holds
recording, quality, permission checks and quit. When you want the visual controls, pick
"Open control window…" from the menu; closing that window only hides it and does not quit
the app (use "Quit" in the menu for that).

### Control window

Hidden by default — open it with "Open control window…" in the menu. Closing it hides the
window without quitting.


| Control | Behaviour |
| --- | --- |
| Start recording (full screen) | Starts a capture; greyed out and labelled "Recording…" while active |
| Select region | Enters the full-screen picker; greyed out while recording |
| Stop recording | Enabled only while recording; greyed out otherwise |
| Audio | Off / system audio / microphone / system + microphone |
| Quality / Frame rate / Resolution | Smaller · Balanced · High · Max / 30 · 60 fps / 1x · 2x |
| Reveal in Finder | Selects the most recent recording in Finder |
| Request / check permission | Triggers the system prompt and reports the result |

The status line at the bottom shows `Ready`, `● Recording 00:12`, `Saving…`, or
`Saved xxx (12.3 MB)`.

### Global hotkeys

| Hotkey | Action |
| --- | --- |
| `⇧⌘R` | Start / stop full-screen recording |
| `⇧⌘E` | Open the region picker; press once more while recording to stop |
| `⌘Q` | Quit when the control window is active (the current file is finalised first) |

### Region recording

1. Press `⇧⌘E` or click "Select region", then drag out the target area (any key cancels).
2. On mouse-up the **selection frame stays on screen**, with a control bar above it:
   `width × height · timer`, `Start recording`, `Close`.
3. Drag the control bar to move the region; drag the round grip at the bottom-right to resize it.
4. Click `Start recording` on the bar: the frame turns red, the timer runs, and the button
   becomes `Stop recording`.
5. Click `Stop recording` to finish. The frame remains in place so the same region can be
   recorded again immediately.

Moving or resizing during a recording does **not** change the captured area of the running
recording (this avoids jumps in the video). The new region applies to the **next**
recording, and the control bar shows a hint for that.

### Menu bar & settings

The menu offers, in order: start recording (full screen), select record region, stop recording,
audio (four-option submenu), quality (Smaller / Balanced / High / Max), frame rate (30 / 60), resolution
(1x / 2x), reveal latest recording, open control window…, request / check permission, quit.
"Stop recording" is enabled only while recording and greyed out otherwise. Menu items and
window controls share the same settings.

## Output & measured size

| Item | Value |
| --- | --- |
| Container / codecs | MP4, HEVC video (hardware), AAC 128 kbps / 48 kHz stereo audio |
| Sampling | YUV420 (`420v`, NV12) |
| Defaults | Balanced quality, 30 fps, 2x (Retina pixels), system audio on |
| Naming | `iRec-YYYYMMDD-HHMMSS.mp4`, with a numeric suffix on collision |
| Location | `~/Desktop` (`Settings::out_dir`) |
| Log | `~/Library/Logs/irec.log` |

Measured on a MacBook built-in display at 1728×1117, 30 fps, Balanced quality, 10 s clip:

| Resolution | Actual pixels | 10 s size | Per minute |
| --- | --- | --- | --- |
| 2x (default, high) | 3456×2234 | 3.49 MB | ≈ 21 MB |
| 1x (space saving) | 1728×1116 | 0.96 MB | ≈ 5.8 MB |

## Audio

| Mode | Meaning |
| --- | --- |
| Off | Video only, no audio track |
| System audio | What applications play |
| Microphone | Microphone only |
| System + microphone | Both PCM streams are converted to 48 kHz stereo and summed into one track |

Selecting a microphone mode requests microphone access right away (macOS shows its prompt
the first time). Built-in microphones deliver a low level (measured peak around -54 dB), so
the mic is boosted about +10 dB before mixing (`MIC_GAIN` in `src/audio.rs`; raise it if you
need more). If no microphone sample arrives within two seconds, `~/Library/Logs/irec.log`
records a "microphone received no samples" note — usually a missing permission or a silent
input device. Microphone samples come from ScreenCaptureKit's
`SCStreamOutputType::Microphone` in the device's native PCM format (measured: 48 kHz, mono,
float32); `src/audio.rs` resamples them, expands them to stereo and mixes them into the
system-audio sample buffers. In microphone-only mode the system samples are silenced before
mixing.

## Permissions & troubleshooting

Screen Recording permission is requested only when a capture actually starts: on macOS 15
`CGPreflightScreenCaptureAccess()` reports "not granted" incorrectly, so requesting at
launch would make the system prompt again and again. Use "Request / check permission" in
the control window at any time.

If it still reports no permission:

1. Quit iRec completely (⌘Q or the Quit button).
2. System Settings → Privacy & Security → Screen Recording: toggle iRec off and on
   (enable every entry if there are several; otherwise add `/Applications/iRec.app` with `+`).
3. Relaunch iRec and click "Request / check permission" to see the verdict.
4. Fallback: run `/Applications/iRec.app/Contents/MacOS/iRec &` in Terminal — if Terminal
   already has permission, recording works with window and hotkeys intact.

Changing the signature or the bundle identifier (`com.nows.irec`) makes macOS treat the app
as new, so permission has to be granted once again. Also, macOS only applies a grant after
iRec is **fully quit and relaunched** — closing the window is not enough. The same applies to the microphone:
System Settings → Privacy & Security → Microphone, allow iRec and relaunch.

## Headless self-test

Verify capture and muxing without the UI:

```sh
cargo run --release -- --selftest --seconds 5                             # full screen
cargo run --release -- --selftest --seconds 5 --region 200,300,900,600    # region (Cocoa coords, y from the bottom)
cargo run --release -- --selftest --seconds 5 --no-audio --scale 1        # no audio, 1x
cargo run --release -- --selftest --seconds 5 --audio mic                 # microphone only
cargo run --release -- --selftest --seconds 5 --audio both                # system + microphone
```

Options: `--seconds`, `--region x,y,w,h`, `--fps`, `--scale`, `--out <dir>`,
`--audio off|system|mic|both` (`--no-audio` equals `off`).
Set `SCREEN_REC_DEBUG=1` for capture/encoding diagnostics on stderr.

## Architecture notes

| Module | Responsibility |
| --- | --- |
| `src/recorder.rs` | ScreenCaptureKit capture + `AVAssetWriter` (HEVC/AAC) muxing |
| `src/frame.rs` | Copies frames into an owned `CVPixelBufferPool` and crops to the region |
| `src/hotkey.rs` | Carbon `RegisterEventHotKey`; no Accessibility or Input Monitoring permission needed |
| `src/overlay.rs` | Full-screen transparent `NSWindow` with a custom `NSView` for drag selection |
| `src/hud.rs` | Persistent selection frame: outline window, control bar and resize grip working together |
| `src/audio.rs` | PCM read-out, resampling and mixing (microphone into system audio) |
| `src/state.rs` | Global settings and command queue (hotkeys, menu, picker all executed on the main-thread timer) |

Region recording captures the whole display and crops while copying frames, rather than
feeding ScreenCaptureKit sub-region frames straight into `AVAssetWriter`, which fails with
`AVFoundationErrorDomain -11800` / `OSStatus -16122`.

The selection frame and control window are marked `NSWindowSharingType::None`, and the
capture filter additionally excludes this process' windows, so no UI element ends up in
the recording.

## Reliability

- **Fragmented writing**: a movie fragment is flushed every 5 seconds. If the app is
  force-quit or crashes, the file still contains moov plus written fragments and stays
  playable instead of becoming an unopenable stub.
- **Finalising off the main thread**: stopping waits for `AVAssetWriter` on a background
  thread while the run loop keeps going, so the file always gets closed.
- **Finalise on quit**: quitting or terminating writes out the file that is in progress.
- **Fail fast on encoder errors**: an encoder error ends the recording immediately and
  reports it, instead of continuing to write a broken file.

## Known limitations

- Only the main display is recorded.
- Release bundles are not notarised, so other machines need the quarantine workaround above.
- System audio (what applications play) is captured; microphone input is not.
- Hotkeys are fixed to `⇧⌘R` / `⇧⌘E` (see `register_defaults` in `src/hotkey.rs`).
- Adjusting the selection during a recording does not change the framing of the current video.

## App icon

`resources/icon.svg` is the single source (dark squircle, screen panel, selection corners,
red record dot). `resources/AppIcon.icns` is generated from it and bundled by
`scripts/build_app.sh`. Regenerate with headless Chrome — `qlmanage` bakes in a white
background:

```sh
mkdir -p /tmp/icon.iconset
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new \
  --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=1024,1024 --default-background-color=00000000 \
  --screenshot=/tmp/icon1024.png "file://$PWD/resources/icon.svg"
# then produce the 10 iconset sizes with PIL / sips, and finally:
iconutil -c icns /tmp/icon.iconset -o resources/AppIcon.icns
```
