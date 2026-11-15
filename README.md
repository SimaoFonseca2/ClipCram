# ClipCram

ClipCram squeezes clips under Discord's upload limit. Drop in your videos, trim and edit them on a timeline, pick a size limit, and it encodes each clip to land just under that limit while looking as good as the bitrate allows. FFmpeg does the encoding; ClipCram works out the settings, checks the result, and retries if needed.

## Features

**Queue**
- Add videos by dragging files or whole folders onto the window, with the Add videos button, or by passing them on the command line (`clipcram clip1.mp4 clip2.mkv`), which also makes "Open with" in your file manager work.
- Reads MP4, MKV, MOV, AVI, WebM, M4V, TS, M2TS, MTS, FLV, WMV, 3GP, MPG and OGV.
- Each video keeps its own trim, encoding and output settings. "Apply these settings to all videos" copies them across.
- Encodes the queue one video at a time, with speed and time remaining. Cancel stops the current video; a failed video doesn't stop the rest.
- When some videos are already done, you can encode only the rest.
- **Watch folder:** point it at your OBS replay-buffer or ShadowPlay folder and new clips join the queue as they're saved (once the file stops growing), optionally encoding right away. New clips never steal the selection from the video you're editing.
- The queue, with every trim and edit, comes back the next time you open the app.
- Your source files are never modified, and existing files are never overwritten (a `_1`, `_2`… suffix is added instead).

**Preview and editing**
- Built-in player with play/pause, seeking, frame stepping, volume and mute.
- Timeline with thumbnails, the audio waveform and a time ruler. Ctrl+scroll (or pinch) zooms in for precise trims; scrolling sideways pans.
- Draggable start and end handles, and start/end fields that accept seconds (`83.5`) or clock time (`1:23.5`).
- **Cut out parts:** press C where a part starts and again where it ends to remove it, as many times as you like. The parts are joined seamlessly.
- **Mute parts:** M works the same way to silence a part without cutting it.
- **Crop:** free or 16:9, 9:16, 1:1, 4:3 and 4:5, adjusted by dragging the box on the preview. Cropping also frees up bitrate for the part you keep.
- **Speed:** 0.25× to 4×, with the audio's pitch kept.
- **Caption:** text at the top or bottom, in three sizes.
- **Undo and redo** for all of the above (Ctrl+Z, Ctrl+Shift+Z or Ctrl+Y).
- **Preview the result:** encodes three seconds from the middle with the real settings and bitrate, then shows a before/after comparison: the original left of a line you drag across the picture, the encode right of it, with its size and quality score. Step through it frame by frame, zoom in up to 8× (Ctrl+scroll or the 2×/4× buttons, scroll to pan) to inspect detail, or play the sample. Change a setting and press "Try again" to compare the new result at the same spot.

| Key | Action |
|---|---|
| Space or K | Play or pause |
| I / O | Set start / end at the playhead |
| C | Start, then finish, a part to cut out |
| M | Start, then finish, a part to mute |
| Esc | Cancel a part you've started marking |
| ← / → or J / L | Jump 5 seconds (1 second with Shift for the arrows) |
| , / . | Step one frame back / forward |
| Home / End | Jump to the start / end point |
| Ctrl+Z / Ctrl+Shift+Z | Undo / redo |

**Encoding**
- **Size limit:** 10 MB (free), 50 MB (Nitro Basic), 500 MB (Nitro), email (17 MiB, which fits a 25 MB attachment limit once mail encoding adds its third) or a custom size. Remembered between runs.
- **Format:** MP4, GIF or animated WebP. GIF and WebP have no sound; ClipCram adjusts their size, frame rate and quality until they fit.
- **Encode on:** the CPU (best quality) or any GPU in your computer, listed by name (for example "AMD Radeon RX 7800 XT"). Every GPU is found, including a second card or an integrated GPU, and each is test-encoded at startup so only working ones appear. Each vendor gets its own quality settings (B-frame references on NVIDIA, lookahead on Intel, pre-analysis on AMD's AMF, the best quality level on VA-API), with a fallback to basic settings on older hardware or drivers.
- **Codec:** H.265, H.264 or AV1, whichever the chosen CPU or GPU supports.
- **Resolution:** Auto, Original, or a fixed 1440p down to 360p. Never upscales.
- **Frame rate:** Auto, Original, up to 60 fps, or up to 30 fps.
- **Speed:** Faster, Balanced (default) or Best quality.
- **Audio:** AAC at 64 to 160 kbps, or no audio. Videos with several audio tracks (such as separate game and mic tracks from OBS) can be mixed into one track, which is the default since Discord only plays the first.
- **HDR to SDR** conversion for HDR phone footage, so colors don't look washed out on Discord.
- **Denoise** (light or strong) for grainy or low-light footage, which leaves more bitrate for detail.
- **Loudness:** even out quiet and loud clips to -16 LUFS. When mixing tracks, set each one's volume (for example the game against your mic); the preview plays the same mix.
- **Presets:** save the encoding settings and size limit under a name and apply them with one click.
- **Already fits?** A clip that's already under the limit, in a codec Discord plays and with nothing to change, is copied as-is: no quality loss and done in a second.
- **Quality score:** after each encode the result is compared with the source (SSIM) and rated Excellent, Good, Fair or Soft.
- Settings are remembered: new videos start with whatever you chose last.
- A live estimate of the resolution, frame rate and bitrate the selected clip will get, with a warning when a clip is too long for the limit to look good.

**Output**
- A default output folder for new videos (your Videos folder to start with), plus a per-video folder and filename.
- Files are named after the source and codec, like `clip_discord_h265.mp4`, and follow codec changes until you rename them yourself.
- "Show in folder" for finished files.
- **Copy to clipboard** puts the finished file on the clipboard, ready to paste into Discord (optionally for every video automatically).
- **Discord webhook:** post finished videos straight to a channel (needs `curl`).
- A desktop notification when the queue finishes.
- **Right-click menu:** Settings can add "Squeeze with ClipCram" to the file manager (Dolphin, Nautilus and Nemo on Linux, Explorer on Windows). It encodes with your saved settings without opening the window.

**Encoding on the GPU**
- Pick where to encode under "Encode on": the CPU (best quality) or any GPU in your computer, by name ("AMD Radeon RX 7800 XT"). Every GPU is found, including a second card or an integrated GPU.
- Each vendor gets its own quality settings: B-frame references on NVIDIA, lookahead and adaptive keyframes on Intel, pre-analysis and high-quality VBR on AMD's AMF, the best quality level on VA-API, and quality-first encoding on Apple. Older hardware or drivers that reject these fall back to basic settings automatically.
- GPU encodes that land well under the limit are encoded again at a corrected bitrate, using the space that would otherwise go to waste.
- GPU detection results are remembered between launches (and checked again in the background), so a GPU codec is ready as soon as the window opens.

## How it fits clips under the limit

1. **Budget.** It takes 93% of the size limit (87% for single-pass encoders, which land less precisely), subtracts the audio for every track that will be kept, and spreads the rest over the clip's length to get a video bitrate.
2. **Size and frame rate.** Auto resolution picks the largest size that still gets enough bits per pixel for the chosen codec, since a smaller sharp picture looks better than a large blocky one. Auto frame rate keeps the full frame rate unless the picture would otherwise drop below 480p, then halves it by dropping whole frames (60 to 30, 50 to 25) so motion stays even.
3. **Encode.** x264, x265 and libaom run a real two-pass encode with a fast analysis pass. SVT-AV1, rav1e and GPU encoders run a single pass. There's no tight bitrate cap, so fast action can borrow bits from calm scenes.
4. **Check and retry.** The finished file is measured. If it's over the limit, the bitrate is scaled down and it tries again, up to five attempts. A retry that keeps the same resolution reuses the first-pass analysis and only re-runs the second pass.
5. **Use leftover space.** With Best quality, a two-pass result that lands under 90% of the limit is encoded again at a higher bitrate, and the larger file that still fits is kept.

Along the way it also:
- Outputs 8-bit 4:2:0 video for H.264 and H.265 so every player can handle it, even from 10-bit or 4:4:4 sources, and 10-bit for AV1 to reduce banding.
- Respects phone rotation, so portrait videos aren't squashed.
- Downmixes surround audio to stereo, and skips cover art embedded as a video stream.
- Tags HEVC so it plays on Apple devices, and puts the index at the front of the file so Discord can start playing before it finishes downloading.
- Uses hardware decoding for the source when it's available, falling back to the CPU if a file trips it up.
- Writes to a hidden temporary file and only renames it when it's done, so a crash never leaves a broken file under the final name.

## Performance

These numbers come from `bench/bench.sh`:
- **Clip:** 5 seconds of 720p60 (a fractal zoom with film grain, which is hard to compress) with two audio tracks.
- **Size limit:** 1.6 MiB, the same bitrate a 30-second clip gets at the 10 MB setting.
- **Machine:** a single-core Linux VM with FFmpeg 6.1. A normal multi-core PC is several times faster.
- **Quality:** SSIM and PSNR measured against the source. Higher is better.

| Codec | Speed | Time | Limit used | SSIM | PSNR |
|---|---|---|---|---|---|
| H.265 | Faster | 29.7 s | 91.8% | 0.9688 | 33.64 dB |
| H.265 | Balanced | 54.5 s | 93.4% | 0.9715 | 33.93 dB |
| H.265 | Best quality | 203.5 s | 94.5% | 0.9724 | 34.06 dB |
| H.264 | Balanced | 21.0 s | 94.5% | 0.9663 | 33.07 dB |
| AV1 (SVT-AV1) | Balanced | 63.9 s | 88.8% | 0.9739 | 34.60 dB |
| H.265, 10-bit source | Balanced | 55.9 s | 91.9% | 0.9709 | 33.81 dB |

Every encode fit on the first attempt.

## Command line

`clipcram encode` encodes without opening the window. Every edit is available:

```
clipcram encode --size 9.5 --codec h264 clip.mkv
clipcram encode --saved --notify *.mp4                     # the app's saved settings
clipcram encode --start 1:05 --end 1:40 --cut 1:12-1:20 --crop 9:16 \
                --text "clutch" --playback-speed 1.5 --on gpu clip.mkv
clipcram encode --format gif --size 8 --end 4 reaction.mp4
```

Run `clipcram encode --help` for all options. `CLIPCRAM_DEBUG=1` prints every FFmpeg command.

## Requirements

FFmpeg and FFprobe. ClipCram looks for them next to its own executable first (so you can ship them together), then on your `PATH`, then in the usual install folders (`/usr/local/bin`, `/opt/homebrew/bin`, and so on).

- Linux: install `ffmpeg` with your package manager.
- macOS: `brew install ffmpeg`.
- Windows: `winget install ffmpeg`.

## Building

You need Rust 1.88 or newer (`rustup update` gets the latest). On Linux you also need the ALSA headers for the preview audio:

| Distro | Command |
|---|---|
| Debian / Ubuntu / Mint | `sudo apt install build-essential pkg-config libasound2-dev` |
| Fedora | `sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel` |
| Arch | `sudo pacman -S base-devel alsa-lib` |

Windows and macOS need nothing extra. Then build:

```
cargo build --release
```

The program ends up at `target/release/clipcram` (`.exe` on Windows). It's a single file you can copy anywhere. `Cargo.lock` is included so you get the exact library versions it was tested with.

### Arch Linux and CachyOS

To build a `clipcram` package from your checkout and install it with pacman, including the app menu entry and icon, run this in the repository root:

```
makepkg -si
```

It builds the code exactly as it is on disk, uncommitted changes included. The package version comes from git, for example `1.0.0.r3.gabc1234` for three commits after the `v1.0.0` tag. makepkg's work files go into `.makepkg/` rather than `src/`, so cleaning up with `makepkg -c` never touches the source code.

## Testing

- `cargo test` runs the unit tests.
- `DC_TEST_INPUT=/path/to/clip.mkv cargo test --release -- --ignored --nocapture` runs real encodes of one of your own clips with each codec.
- `bash bench/bench.sh` reproduces the performance table on your machine.
- `bash bench/e2e.sh` runs real encodes of every editing feature (cuts, speed, crop, mute, track volume, loudness, captions, GIF, WebP, copying, HEVC, AV1) and checks each result with ffprobe.
- GitHub Actions run clippy and the tests on Linux, Windows and macOS for every push, and build downloadable binaries for a tag like `v1.1.0` (`.github/workflows/`).

## Notes

- **Codec compatibility on Discord:** H.264 plays inline for everyone. H.265 doesn't for some viewers (notably some Linux and Firefox users), who have to download it instead. AV1 gives the sharpest result per MB and plays in the Discord desktop app and browsers, though some older phones can't play it.
- **Wayland:** on Linux, ClipCram runs through XWayland, because the windowing library can't receive dropped files on native Wayland yet. Set `CLIPCRAM_WAYLAND=1` to run natively and use the Add videos button instead.
- **Webhook and update check** use `curl`, which comes with Windows 10 and later, macOS and practically every Linux system.
- **Notifications** use `notify-send` (libnotify) on Linux, Notification Center on macOS and a toast on Windows.
- **File dialogs on Linux** use the desktop portal (`xdg-desktop-portal`), which GNOME, KDE and most desktops already run.
- **GPU encoders** are much faster but need more bitrate for the same quality, so short size limits look softer than with the CPU encoders. They only appear in the list if a quick test encode succeeds on your machine.

## License

ClipCram is released under the [MIT License](LICENSE).

It runs FFmpeg as a separate program and doesn't include it. FFmpeg has its own license (GPL for most builds, since they include x264 and x265), so if you ship FFmpeg alongside ClipCram, include FFmpeg's license and a link to its source.

ClipCram isn't affiliated with or endorsed by Discord.
