# iRec

A menu-bar recorder and screenshot tool for macOS, written in Rust. Capture the full
screen or a selected region with optional audio, grab screenshots, annotate them, and
hang the result on a clothesline under the menu bar.

[简体中文](./README.zh.md) | English

Project page: <https://nowszhao.github.io/iRec/>

## Features

- **Recording** — full screen or a drag-selected region, HEVC hardware encoding, MP4
  (HEVC + AAC). Quality, frame rate (30 / 60) and resolution (1x / 2x) are adjustable.
- **Audio** — off / system audio / microphone / system + microphone, each on its own track.
- **Screenshots** — full screen or the selected region, saved as PNG.
- **Annotation** — arrows, boxes and text drawn in iRec; move, resize and recolour any
  mark, then write back over the original PNG.
- **The clothesline** — results hang under the menu bar: rest the pointer there to drop
  the line, then click, double-click or drag a card wherever you need it.
- **Quiet by default** — no Dock icon, no window on launch, everything in the status menu.
- **Never in the shot** — the control window, selection frame and the line use
  `NSWindowSharingType::None`, and the line tucks away while recording.
- **Auto-update** — checks GitHub Releases on its own and swaps `iRec.app` on request.
- **Command line** — `iRec --shot`, `iRec --update`, `iRec --selftest`.

## Requirements

macOS 13.0 or later, plus the Screen Recording permission (requested on the first real
capture — see [Permissions](#permissions)). Building needs a Rust toolchain (edition 2024)
and the Xcode command line tools.

## Install

Download the package for your Mac from
[Releases](https://github.com/nowszhao/iRec/releases):

| Mac | Package |
| --- | --- |
| Apple Silicon (M1 / M2 / M3 / M4 …) | `iRec-<version>-arm64.zip` |
| Intel | `iRec-<version>-x86_64.zip` |

Run `uname -m` if unsure. Unzip and drag `iRec.app` into Applications.

Release bundles are ad-hoc signed (no Developer ID is configured in CI), so Gatekeeper may
block the first launch: right-click → Open, or run
`xattr -dr com.apple.quarantine /Applications/iRec.app`.

### Build from source

```sh
git clone ssh://git@ssh.github.com:443/nowszhao/iRec.git && cd iRec

./scripts/build_app.sh            # release build → target/release/iRec.app
ARCH=x86_64 ./scripts/build_app.sh   # arm64 | x86_64 | universal
./scripts/package_release.sh      # one zip per architecture + SHA256SUMS into dist/
```

Sign with a stable certificate to keep the Screen Recording grant across rebuilds — ad-hoc
signing (the default) gives every build a new code identity:

```sh
SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" ./scripts/build_app.sh
```

Pushing a `v*` tag makes GitHub Actions build both architectures and publish the release.

## Usage

| Hotkey | Action |
| --- | --- |
| `⇧⌘R` | Start / stop full-screen recording |
| `⇧⌘E` | Open the region picker (press again while recording to stop) |
| `⇧⌘S` | Screenshot the full screen |
| `⇧⌘A` | Screenshot the selected region (pick one first if needed) |
| `⌘Q` | Quit, after finalising the file in progress |

Click the menu bar item for recording, audio, quality, save location, the line and
permission checks. "Open control window…" brings up the same controls in a window; closing
it only hides it.

Selecting a region leaves a frame on screen with its own control bar: drag the bar to
move it, drag the grip to resize it, and use **Screenshot** or **Start recording** there.
Changing the region during a recording applies to the next one, so the current video never
jumps.

## Screenshots and annotation

Screenshots are saved as `iRec-shot-YYYYMMDD-HHMMSS.png` and hung on the line. Annotate
straight after capturing, or via "Annotate latest screenshot…" in the menu / the Annotate
button on an image card.

| Control | Behaviour |
| --- | --- |
| Arrow / box | Drag one out; the tool returns to Select so you can move it at once |
| Text | Click, type, press Return |
| Select | Click a mark to select it, then drag to move |
| Resize | Drag the white handles: both ends of an arrow, a box corner, the end of a text |
| Colour | Recolours the selected mark |
| `⌫` / undo | Delete the selected mark / the last one |
| `⌘↩` / `Esc` | Save over the original PNG (size and resolution unchanged) / discard |

macOS' own Markup is a sharing service and fails to come up on many machines, so iRec
ships its own editor.

## The clothesline

Finished recordings and screenshots hang from a rope under the menu bar, pegged with a
play button on the first frame.

The line rests above the screen edge: rest the pointer in the menu bar for 0.25 s and it
slides down, move away and it tucks back after 0.5 s. It also shows itself for 2.5 s after
a capture and stays hidden while one is running.

| Gesture | Result |
| --- | --- |
| Click / double-click a card | Reveal in Finder / open in the default player |
| Drag into an app or a folder | The destination gets the file, the card stays |
| Drag onto the Dock's Trash | File to the Trash, card leaves the line |
| Click the cross (⌥ + click) | Take it off the line (⌥ also trashes the file) |

A card only leaves the line when you take it off or clear the line — dragging it somewhere
or deleting the file elsewhere keeps it hanging. At most eight hang at once.

The animation idea comes from [Tendedero](https://github.com/alejandrobujan/tendedero)
(MIT), reimplemented with hand-drawn AppKit and its own spring integration in `src/line.rs`.

## Settings and auto-update

Settings (save location, audio source, quality, frame rate, resolution, line behaviour) are
persisted in `NSUserDefaults`. The default save location is `~/Desktop`; recordings are
named `iRec-YYYYMMDD-HHMMSS.mp4`.

Updates are checked 5 seconds after launch and then every 24 hours. When a newer version
exists the menu shows "↑ New version v0.1.x, click to update": clicking downloads the
matching package, swaps `iRec.app` and relaunches. "Check for updates…" does it on demand,
"Check for updates on launch" turns the automatic check off, and `iRec --update` does it
from the shell. The bundle signature changes with every update, so Screen Recording may
need to be granted again.

## Permissions

Screen Recording is requested on the first real capture, not at launch. If recording still
fails:

1. Quit iRec completely.
2. System Settings → Privacy & Security → Screen Recording: toggle iRec off and on
   (enable every entry if there are several).
3. Relaunch iRec — macOS applies a grant only to processes started afterwards.

When a start fails, iRec says so and offers **Relaunch iRec** as the first button; the menu
status line turns into "⚠ Screen Recording permission not in effect — click to relaunch".
The microphone is granted the same way under Privacy & Security → Microphone.

Two things make a grant look revoked: rebuilding (ad-hoc signing creates a new code
identity each time) and changing the signature or the bundle id `com.nows.irec`.

## Command line

```sh
iRec --shot [--region x,y,w,h] [--scale 2] [--out <dir>]   # screenshot (y from the bottom)
iRec --update                                              # check for updates
iRec --selftest --seconds 5                                # capture without the UI
```

`--selftest` also takes `--fps`, `--scale`, `--out`, `--audio off|system|mic|both` and
`--region`; set `SCREEN_REC_DEBUG=1` for capture diagnostics. The log is at
`~/Library/Logs/irec.log`.

## Output

| Item | Value |
| --- | --- |
| Container / codecs | MP4, HEVC video (hardware), AAC 128 kbps / 48 kHz stereo |
| Sampling | YUV420 |
| Defaults | Balanced quality, 30 fps, 2x (Retina pixels), system audio on |
| Measured size | ≈ 21 MB per minute at 3456×2234, Balanced quality |

Movies are flushed as fragments every 5 seconds, so a crash leaves a playable file instead
of an unopenable stub. Stopping finishes the file on a background thread, and quitting
writes out whatever is in progress.

## Known limitations

- Only the main display is recorded.
- Release bundles are not notarised.
- Hotkeys are fixed (`register_defaults` in `src/hotkey.rs`).
- Changing the region during a recording applies to the next one.
- If a drop destination moves the file, the card keeps pointing at the old path.

## License

MIT — see [LICENSE](./LICENSE).
