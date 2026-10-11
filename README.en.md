# iRec

A menu-bar screen recorder for macOS, written in Rust. Full-screen or region capture
with optional system audio, encoded as HEVC + AAC in an `.mp4` container and saved to
the desktop by default.

[简体中文](./README.md) | English

Project page: <https://nowszhao.github.io/iRec/>

## Contents

- [Features](#features)
- [Requirements](#requirements)
- [Install & Build](#install--build)
- [Auto-update](#auto-update)
- [Usage](#usage)
  - [Control window](#control-window)
  - [Global hotkeys](#global-hotkeys)
  - [Region recording](#region-recording)
- [Annotating](#annotating)
  - [The line](#the-line)
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
| Never recorded | The control window, the selection frame and the line's windows use `NSWindowSharingType::None`, so they never appear in the capture |
| Audio | Four modes: off / system audio / microphone / system + microphone (one track each) |
| Save location | `~/Desktop` by default, change it with "Choose save location…" in the menu; persisted across restarts |
| Encoding | HEVC hardware encoding, YUV420 sampling, adjustable quality / frame rate / resolution |
| Output | MP4 (HEVC + AAC) named `iRec-YYYYMMDD-HHMMSS.mp4`, saved to `~/Desktop` |
| Screenshots | `⇧⌘S` full screen; `⇧⌘A` the current selection (or pick one first); the region's control bar has a "Screenshot" button. Saved as PNG (`iRec-shot-YYYYMMDD-HHMMSS.png`) and hung on the line |
| Annotating | Arrows, boxes and text drawn right in iRec; image cards on the line have an "Annotate" button, and the menu has "Annotate latest screenshot…" |
| The line | Finished recordings hang from a clothesline under the menu bar: rest the pointer there to drop it, click a card to reveal it in Finder, drag it into an app or a folder, click the cross to take it off |

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

The published bundles are ad-hoc signed (no Developer ID is configured in CI),
so Gatekeeper may block the first launch: right-click → Open, or run
`xattr -dr com.apple.quarantine /Applications/iRec.app`.

## Auto-update

iRec checks GitHub Releases for a newer version on its own:

- One silent check 5 seconds after launch, then once every 24 hours; "Check for
  updates…" in the menu bar does it on demand
- When a newer version exists the menu bar shows "↑ New version v0.1.x, click to
  update": clicking downloads the matching package, swaps `iRec.app` and relaunches
- Turn the automatic check off with "Check for updates on launch"
- From the shell: `iRec --update`
- Only the running `iRec.app` is replaced; without write permission iRec asks you
  to download manually
- The bundle signature changes after an update, so Screen Recording may need to be
  granted again

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
git tag v0.1.1 && git push origin v0.1.1
```

### Signing and notarisation (for distribution)

The workflow signs with a Developer ID certificate and notarises. Add these
repository secrets (Settings → Secrets and variables → Actions):

| Secret | Meaning |
| --- | --- |
| `MACOS_CERTIFICATE` | base64 of the Developer ID Application `.p12` |
| `MACOS_CERTIFICATE_PASSWORD` | password of that `.p12` |
| `KEYCHAIN_PASSWORD` | password for the temporary CI keychain (any string) |
| `DEVELOPER_ID_APPLICATION` | signing identity, e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID` / `APPLE_TEAM_ID` / `APPLE_APP_PASSWORD` | Apple ID + app-specific password for notarisation |
| or `APPLE_API_KEY_ID` / `APPLE_API_ISSUER` / `APPLE_API_PRIVATE_KEY` | App Store Connect API key (base64 `.p8`) instead |

Flow: `codesign --options runtime --timestamp` with `resources/iRec.entitlements`
(hardened runtime needs `com.apple.security.device.audio-input` for the mic) → zip →
`xcrun notarytool submit --wait` → `xcrun stapler staple`.

Without those secrets the job still succeeds and falls back to ad-hoc signing (not
notarised, so the quarantine workaround above is needed). You can also run
`./scripts/sign_and_notarize.sh dist/arm64/iRec.app` locally.

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
   `width × height · timer`, `Screenshot`, `Start recording`, `Close`.
3. Drag the control bar to move the region; drag the round grip at the bottom-right to resize it.
4. Click `Start recording` on the bar: the frame turns red, the timer runs, and the button
   becomes `Stop recording`. Click `Screenshot` instead to capture that area as a PNG — the
   frame stays put, so the same region can be recorded and captured in turn.
5. Click `Stop recording` to finish. The frame remains in place so the same region can be
   recorded again immediately.

Moving or resizing during a recording does **not** change the captured area of the running
recording (this avoids jumps in the video). The new region applies to the **next**
recording, and the control bar shows a hint for that.

### Annotating

After a screenshot (or from the "Annotate" button on an image card, or "Annotate latest
screenshot…" in the menu) the annotation window opens:

One row of icons along the bottom: **Select** (the default), arrow, box, text, then four
colours, then undo, cancel and done (`⌘↩`).

| Control | What it does |
| :-- | :-- |
| Arrow / Box | Drag out a line or a rectangle; the tool returns to Select so you can move it |
| Text | Click on the image, type, press Return to confirm |
| Select | Click a mark to select it (dashed outline), then drag to move it |
| Resize | Drag a white handle: both ends of an arrow, the bottom-right of a box, the right end of a text (further out makes it bigger) |
| Colour | With a mark selected, a click recolours it |
| `⌫` / Undo | Deletes the selected mark / the last one |
| Done `⌘↩` / Cancel `Esc` | Saves over the original PNG (thumbnail updates at once) / discards |

The canvas is dimmed with the image on a soft shadow and the bar is icon-only, following the
editors in CleanShot and Snipaste.

macOS' own Markup is registered as a sharing service and will not come up on many machines,
so iRec ships its own. Saving writes back over the file, keeping its size and resolution.

### The line

A finished recording is more than a line of text in the menu: it hangs from a clothesline
just below the menu bar, its first frame with a play button, pegged to the rope, flown up
from the area you just recorded.

The line rests above the top edge. Rest the pointer in the menu bar for 0.25 s and it slides
down; move away and it tucks itself back after 0.5 s. It also shows its face for 2.5 s right
after a recording, stays tucked away while one is running, and is never recorded.

| Gesture | Result |
| :-- | :-- |
| Click a card | Reveal it in Finder |
| Double click | Open it in the default player |
| Drag into another app | It gets a copy, the card stays on the line |
| Drag into a folder, the Desktop or a dialog | The file stays there, the card stays on the line |
| Drag onto the Dock's Trash | The file goes to the Trash, the card leaves the line |
| Click the cross | Take it off the line (the file stays put) |
| ⌥ + click the cross | Take it off and move the file to the Trash |

What is dragged out is a real file: the file URL is handed to AppKit's dragging session and the
destination decides what happens with it. Apps take a copy, Finder keeps the file in the folder,
and the Dock's Trash is followed up by iRec moving the file itself.

**A card only leaves the line in two cases**: clicking the cross (⌥ + click also sends the file to
the Trash), or "Clear the line" in the menu. Dragging a recording into a folder or a dialog keeps
it hanging, and so does the file being deleted by another app — clicking it then just says
"file is no longer there".

At most eight hang at a time; older ones fall off, or take everything down with "Clear the line"
in the menu. Related items: Show / hide the line, Bring the line down when the pointer rests in
the menu bar, Hang recordings on the line, Clear the line.

The animation idea comes from [Tendedero](https://github.com/alejandrobujan/tendedero) (MIT):
a rope that fades out at both ends, the swing that overshoots and settles on a damped spring,
a breeze that runs along the whole row now and then, the glass frame and peg that grow in while
a card flies up, and the fall that spins a discarded card away. iRec reimplements that motion
with hand-drawn AppKit and its own spring integration (`src/line.rs`), hanging recordings
instead of screenshots.

### Menu bar & settings

The menu offers, in order: start recording (full screen), select record region, stop recording,
screenshot (full screen), screenshot (region), annotate latest screenshot…, open Markup after a
screenshot, audio (four-option submenu), quality
(Smaller / Balanced / High / Max), frame rate (30 / 60), resolution
(1x / 2x), show / hide the line, bring the line down when the pointer rests in the menu bar, hang
recordings on the line, clear the line, reveal latest recording, open control window…, request /
check permission, quit.
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

## Save location

The default is `~/Desktop`. The menu offers:

- `Save location: ~/Desktop` (greyed out, just shows the current path)
- `Choose save location…`: opens the system directory panel (folders can be created)
- `Reset to Desktop`: back to `~/Desktop`

The control window has a "Save location: … (click to change)" button as well.

Settings are stored in `NSUserDefaults` (`com.nows.irec`), so they **survive a
restart**; besides the save location, the audio source, quality, frame rate and
resolution are remembered too. To clear them: `defaults delete com.nows.irec`.

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

**Already enabled in Settings, yet recording fails with "the user denied … TCC"**: that is
macOS applying a grant only to processes started *after* it. When a start fails now, iRec says
so directly and offers "Relaunch iRec" as the first button (it quits and reopens itself a second
later); the menu status line turns into "⚠ Screen Recording permission not in effect — click to
relaunch" and does the same when clicked. Once the system grant lands, the line switches to
"✓ Permission granted — relaunch iRec for it to take effect".

Two things make it look revoked again:

- Rebuilding: ad-hoc signing (the default) gives every build a new code identity, voiding the
  previous grant. Sign with `SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" ./scripts/build_app.sh`
  to keep a stable identity and stop re-authorising.
- Changing the signature or the bundle identifier (`com.nows.irec`).

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

Screenshots work from the command line too:

```sh
cargo run --release -- --shot --out /tmp/shots                            # full screen
cargo run --release -- --shot --region 200,417,600,400 --scale 2          # region (y from the bottom)
```

## Architecture notes

| Module | Responsibility |
| --- | --- |
| `src/recorder.rs` | ScreenCaptureKit capture + `AVAssetWriter` (HEVC/AAC) muxing |
| `src/frame.rs` | Copies frames into an owned `CVPixelBufferPool` and crops to the region |
| `src/hotkey.rs` | Carbon `RegisterEventHotKey`; no Accessibility or Input Monitoring permission needed |
| `src/overlay.rs` | Full-screen transparent `NSWindow` with a custom `NSView` for drag selection |
| `src/hud.rs` | Persistent selection frame: outline window, control bar and resize grip working together |
| `src/line.rs` | The line: two `NSPanel`s (rope and flight) with hand-drawn cards, springs integrated per frame |
| `src/shot.rs` | Screenshots: `SCScreenshotManager` frame → PNG, then hung on the line |
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
- The line hangs on the screen under the pointer, and the peek after a recording happens there too.
- The drop destination decides: dropping into a folder may copy or move. If it moves, the card
  still points at the old path and clicking it says "file is no longer there" — take it off with
  the cross, or use "Clear the line".

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
