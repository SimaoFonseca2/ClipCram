# ClipCram

ClipCram squeezes clips under Discord's upload limit. Drop in your videos, trim them on a timeline, pick a size limit, and it encodes each clip to land just under that limit while looking as good as the bitrate allows. FFmpeg does the encoding; ClipCram works out the settings, checks the result, and retries if needed.

## Features

**Queue**
- Add videos by dragging files or whole folders onto the window, with the Add videos button, or by passing them on the command line (`clipcram clip1.mp4 clip2.mkv`), which also makes "Open with" in your file manager work.
- Reads MP4, MKV, MOV, AVI, WebM, M4V, TS, M2TS, MTS, FLV, WMV, 3GP, MPG and OGV.
- Each video keeps its own trim, encoding and output settings. "Apply these settings to all videos" copies them across.
- Encodes the queue one video at a time, with speed and time remaining. Cancel stops the current video; a failed video doesn't stop the rest.
- When some videos are already done, you can encode only the rest.
- Your source files are never modified, and existing files are never overwritten (a `_1`, `_2`… suffix is added instead).

**Preview and trimming**
- Built-in player with play/pause, seeking, frame stepping, volume and mute.
- Timeline with draggable start and end handles, and start/end fields that accept seconds (`83.5`) or clock time (`1:23.5`).
- "Set start here", "Set end here", "Preview clip" (plays just the trimmed part) and "Use whole video".

| Key | Action |
|---|---|
| Space | Play or pause |
| I / O | Set start / end at the playhead |
| ← / → | Jump 5 seconds (1 second with Shift) |
| , / . | Step one frame back / forward |
| Home / End | Jump to the start / end point |

**Encoding**
- **Size limit:** 10 MB (free), 50 MB (Nitro Basic), 500 MB (Nitro) or a custom size. Remembered between runs.
- **Codec:** H.265, H.264 or AV1, whichever your FFmpeg supports. Working GPU encoders (NVIDIA, AMD, Intel, Apple, VA-API) are detected at startup and listed as fast options.
- **Resolution:** Auto, Original, or a fixed 1440p down to 360p. Never upscales.
- **Frame rate:** Auto, Original, up to 60 fps, or up to 30 fps.
- **Speed:** Faster, Balanced (default) or Best quality.
- **Audio:** AAC at 64 to 160 kbps, or no audio. Videos with several audio tracks (such as separate game and mic tracks from OBS) can be mixed into one track, which is the default since Discord only plays the first.
- **HDR to SDR** conversion for HDR phone footage, so colors don't look washed out on Discord.
- A live estimate of the resolution, frame rate and bitrate the selected clip will get, with a warning when a clip is too long for the limit to look good.

**Output**
- A default output folder for new videos (your Videos folder to start with), plus a per-video folder and filename.
- Files are named after the source and codec, like `clip_discord_h265.mp4`, and follow codec changes until you rename them yourself.
- "Show in folder" for finished files.

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

## Notes

- **Codec compatibility on Discord:** H.264 plays inline for everyone. H.265 doesn't for some viewers (notably some Linux and Firefox users), who have to download it instead. AV1 gives the sharpest result per MB and plays in the Discord desktop app and browsers, though some older phones can't play it.
- **Wayland:** on Linux, ClipCram runs through XWayland, because the windowing library can't receive dropped files on native Wayland yet. Set `CLIPCRAM_WAYLAND=1` to run natively and use the Add videos button instead.
- **File dialogs on Linux** use the desktop portal (`xdg-desktop-portal`), which GNOME, KDE and most desktops already run.
- **GPU encoders** are much faster but need more bitrate for the same quality, so short size limits look softer than with the CPU encoders. They only appear in the list if a quick test encode succeeds on your machine.

## License

ClipCram is released under the [MIT License](LICENSE).

It runs FFmpeg as a separate program and doesn't include it. FFmpeg has its own license (GPL for most builds, since they include x264 and x265), so if you ship FFmpeg alongside ClipCram, include FFmpeg's license and a link to its source.

ClipCram isn't affiliated with or endorsed by Discord.
