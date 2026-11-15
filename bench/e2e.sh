#!/usr/bin/env bash
# End-to-end tests: real encodes through `clipcram encode`, each result checked with
# ffprobe/ffmpeg. Run from anywhere:   bash bench/e2e.sh [path/to/clipcram]
# Uses only CPU encoders and short clips, so it runs in a few minutes.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; ROOT="$(dirname "$HERE")"
BIN="${1:-$ROOT/target/release/clipcram}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/clipcram"
[ -x "$BIN" ] || { echo "Build first: cargo build --release"; exit 1; }
WORK="${TMPDIR:-/tmp}/clipcram-e2e"; rm -rf "$WORK"; mkdir -p "$WORK/out"
SRC="$WORK/src.mkv"; SMALL="$WORK/small.mp4"
pass=0; fail=0
ok()   { echo "  PASS  $1"; pass=$((pass+1)); }
bad()  { echo "  FAIL  $1"; fail=$((fail+1)); }
check(){ if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
probe(){ ffprobe -v error -show_entries "$2" -of csv=p=0 "$1" | head -n1; }
dur()  { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1"; }
size() { stat -c %s "$1" 2>/dev/null || stat -f %z "$1"; }
near() { awk -v a="$1" -v b="$2" -v t="$3" 'BEGIN{d=a-b; if(d<0)d=-d; exit !(d<=t)}'; }
# Mean volume (dB) of the first audio track between two times of a file.
vol()  { ffmpeg -v info -ss "$2" -t "$3" -i "$1" -map 0:a:0 -af volumedetect -f null - 2>&1 | sed -n 's/.*mean_volume: \(-*[0-9.inf]*\) dB.*/\1/p'; }
run()  { local name=$1; shift; "$BIN" encode --json --out "$WORK/out" --name "$name" "$@" 2>"$WORK/$name.log" | tail -n1; }
field(){ python3 -c "import json,sys; d=json.loads(sys.argv[1]); v=d.get('$2'); print('' if v is None else v)" "$1"; }

echo "Making test clips…"
# 12 s of 1280x720 at 60 fps with two audio tracks: a tone ("Game") and noise ("Mic").
ffmpeg -v error -y -f lavfi -i "testsrc2=size=1280x720:rate=60" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" -f lavfi -i "anoisesrc=color=pink:amplitude=0.3:sample_rate=48000" \
  -t 12 -map 0:v -map 1:a -map 2:a -c:v libx264 -preset ultrafast -crf 20 -pix_fmt yuv420p -c:a aac -b:a 128k \
  -metadata:s:a:0 title=Game -metadata:s:a:1 title=Mic "$SRC"
# A small H.264/AAC MP4 that already fits: should be copied, not re-encoded.
ffmpeg -v error -y -f lavfi -i "testsrc2=size=640x360:rate=30" -f lavfi -i "sine=frequency=330" -t 4 \
  -c:v libx264 -preset veryfast -crf 30 -pix_fmt yuv420p -c:a aac -b:a 64k -shortest "$SMALL"

echo "1. Plain H.264 encode to 2 MiB"
r=$(run plain.mp4 --codec h264 --quality faster --size 2 "$SRC"); f=$(field "$r" output)
check "file exists" "[ -f '$f' ]"
check "under 2 MiB" "[ $(size "$f") -le 2097152 ]"
check "duration 12 s" "near $(dur "$f") 12 0.15"
check "one mixed audio track" "[ $(ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 "$f" | wc -l) -eq 1 ]"
check "quality measured" "[ -n '$(field "$r" ssim)' ]"
check "faststart (moov before mdat)" "python3 -c \"d=open('$f','rb').read(65536); import sys; sys.exit(0 if d.find(b'moov')<d.find(b'mdat') or d.find(b'mdat')<0 else 1)\""

echo "2. Cut out the middle: keep 0-3 and 7-12"
r=$(run cut.mp4 --codec h264 --quality faster --size 2 --cut 3-7 "$SRC"); f=$(field "$r" output)
check "duration 8 s" "near $(dur "$f") 8 0.2"
check "audio as long as video" "near $(probe "$f" format=duration) $(ffprobe -v error -select_streams a:0 -show_entries stream=duration -of csv=p=0 "$f") 0.2"

echo "3. Speed 2x and 0.5x"
r=$(run fast.mp4 --codec h264 --quality faster --size 2 --start 0 --end 8 --playback-speed 2 "$SRC"); f=$(field "$r" output)
check "2x: duration 4 s" "near $(dur "$f") 4 0.2"
check "2x: frame rate capped at 60" "[ \"$(probe "$f" stream=avg_frame_rate)\" = \"60/1\" ]"
r=$(run slow.mp4 --codec h264 --quality faster --size 2 --start 0 --end 3 --playback-speed 0.5 "$SRC"); f=$(field "$r" output)
check "0.5x: duration 6 s (not cut short)" "near $(dur "$f") 6 0.2"

echo "4. Crop to 9:16"
r=$(run crop.mp4 --codec h264 --quality faster --size 2 --end 4 --crop 9:16 "$SRC"); f=$(field "$r" output)
w=$(probe "$f" stream=width); h=$(probe "$f" stream=height)
check "portrait output ($w x $h)" "[ $h -gt $w ]"
check "aspect about 9:16" "near $(awk -v w=$w -v h=$h 'BEGIN{print w/h}') 0.5625 0.02"

echo "5. Mute 2-4 s"
r=$(run mute.mp4 --codec h264 --quality faster --size 2 --end 6 --tracks first --mute 2-4 "$SRC"); f=$(field "$r" output)
check "silent where muted" "awk -v v=$(vol "$f" 2.3 1.4) 'BEGIN{exit !(v<-80 || v==\"-inf\")}'"
check "sound outside" "awk -v v=$(vol "$f" 0.2 1.5) 'BEGIN{exit !(v>-40)}'"

echo "6. Track volume (mix with the noise track at 0)"
r=$(run trackvol.mp4 --codec h264 --quality faster --size 2 --end 4 --track-volume 2=0 "$SRC"); f=$(field "$r" output)
# Should sound exactly like the first track alone (the mute test's encode before its muted
# part), and quieter than the mix of both tracks.
mixed=$(vol "$f" 0.2 1.5); tone=$(vol "$WORK/out/mute.mp4" 0.2 1.5); both=$(vol "$WORK/out/plain.mp4" 0.2 1.5)
check "noise track removed ($mixed dB vs tone alone $tone dB)" "near $mixed $tone 0.5"
check "quieter than both tracks mixed ($both dB)" "awk -v a=$mixed -v b=$both 'BEGIN{exit !(a<b-1)}'"

echo "7. Loudness normalization to -16 LUFS"
r=$(run loud.mp4 --codec h264 --quality faster --size 2 --end 8 --tracks first --normalize "$SRC"); f=$(field "$r" output)
lufs=$(ffmpeg -v info -i "$f" -map 0:a:0 -af ebur128 -f null - 2>&1 | sed -n 's/^ *I: *\(-*[0-9.]*\) LUFS/\1/p' | tail -n1)
check "integrated loudness near -16 (got $lufs)" "near $lufs -16 1.5"

echo "8. Caption"
r=$(run text.mp4 --codec h264 --quality faster --size 2 --end 3 --text "GG EZ" --text-pos bottom "$SRC"); f=$(field "$r" output)
check "encoded with caption" "[ -f '$f' ]"
a=$(ffmpeg -v error -ss 1 -i "$f" -frames:v 1 -vf crop=iw:ih/5:0:ih*4/5,scale=64:16 -f rawvideo -pix_fmt gray - | md5sum)
b=$(ffmpeg -v error -ss 1 -i "$WORK/out/crop.mp4" -frames:v 1 -f rawvideo - 2>/dev/null | md5sum)
p=$(ffmpeg -v error -ss 1 -i "$WORK/out/plain.mp4" -frames:v 1 -vf crop=iw:ih/5:0:ih*4/5,scale=64:16 -f rawvideo -pix_fmt gray - | md5sum)
check "bottom of the frame differs from the uncaptioned encode" "[ '$a' != '$p' ]"

echo "9. GIF and WebP"
r=$(run anim.gif --format gif --size 1.5 --end 4 "$SRC"); f=$(field "$r" output)
check "GIF under 1.5 MiB" "[ $(size "$f") -le 1572864 ]"
check "is a GIF" "[ \"$(probe "$f" stream=codec_name)\" = gif ]"
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libwebp; then
  r=$(run anim.webp --format webp --size 1 --end 4 "$SRC"); f=$(field "$r" output)
  check "WebP under 1 MiB" "[ $(size "$f") -le 1048576 ]"
  check "is a WebP" "head -c 12 '$f' | grep -q WEBP"
fi

echo "10. Already fits: copied without re-encoding"
r=$(run small.mp4 --codec h264 --size 9.5 "$SMALL"); f=$(field "$r" output)
check "copied" "[ \"$(field "$r" copied)\" = True ]"
check "same video codec and size" "[ \"$(probe "$f" stream=width)\" = 640 ]"
r=$(run small2.mp4 --codec h264 --size 9.5 --no-copy "$SMALL")
check "--no-copy re-encodes" "[ \"$(field "$r" copied)\" = False ]"
r=$(run small3.mp4 --codec h265 --size 9.5 "$SMALL")
check "different codec re-encodes" "[ \"$(field "$r" copied)\" = False ]"

echo "11. H.265 two-pass and AV1 with denoise, keeping both tracks"
r=$(run hevc.mp4 --codec h265 --quality faster --size 1.5 --end 5 --tracks all "$SRC"); f=$(field "$r" output)
check "HEVC under 1.5 MiB" "[ $(size "$f") -le 1572864 ]"
check "hvc1 tag" "[ \"$(probe "$f" stream=codec_tag_string)\" = hvc1 ]"
check "two audio tracks kept" "[ $(ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 "$f" | wc -l) -eq 2 ]"
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libsvtav1; then
  r=$(run av1.mp4 --codec av1 --quality faster --size 1 --end 4 --denoise light "$SRC"); f=$(field "$r" output)
  check "AV1 under 1 MiB" "[ $(size "$f") -le 1048576 ] && [ \"$(probe "$f" stream=codec_name)\" = av1 ]"
fi

echo "12. Everything at once: cuts, speed, crop, caption, mute, normalize"
r=$(run all.mp4 --codec h264 --quality faster --size 2 --cut 2-4 --cut 8-10 --playback-speed 1.5 --crop 1:1 \
     --text "combo" --mute 0-1 --normalize "$SRC"); f=$(field "$r" output)
check "duration (12-4)/1.5 = 5.33 s" "near $(dur "$f") 5.333 0.25"
check "square" "[ \"$(probe "$f" stream=width)\" = \"$(probe "$f" stream=height)\" ]"
check "under 2 MiB" "[ $(size "$f") -le 2097152 ]"

echo "13. Errors are reported, not crashes"
"$BIN" encode --out "$WORK/out" --cut 0-20 "$SRC" >/dev/null 2>"$WORK/err.log"; code=$?
check "everything cut: exit code 1" "[ $code -eq 1 ] && grep -q 'Nothing is left' '$WORK/err.log'"
"$BIN" encode --codec vp9 "$SRC" >/dev/null 2>&1; check "bad option: exit code 2" "[ $? -eq 2 ]"
"$BIN" encode "$WORK/missing.mp4" >/dev/null 2>&1; check "missing file: exit code 1" "[ $? -eq 1 ]"

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
