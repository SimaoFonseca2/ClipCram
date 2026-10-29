#!/usr/bin/env bash
# Encodes a synthetic test clip with each codec and speed setting, then reports
# encoding time, how much of the size limit was used, and SSIM/PSNR quality.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; ROOT="$(dirname "$HERE")"; cd "$HERE"
T=1.6   # MiB for a 5 s clip: the same bitrate a 30 s clip gets at 9.5 MiB

if [ ! -f clip8.mkv ]; then
  echo "Generating test clips…"
  ffmpeg -v error -y -f lavfi -i "mandelbrot=size=1280x720:rate=60,noise=alls=5:allf=t+u" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000" -f lavfi -i "anoisesrc=color=pink:amplitude=0.2:sample_rate=48000" \
    -t 6.5 -map 0:v -map 1:a -map 2:a -c:v libx264 -preset veryfast -crf 14 -pix_fmt yuv420p -c:a aac -b:a 192k clip8.mkv
  ffmpeg -v error -y -i clip8.mkv -map 0 -c:v libx264 -preset veryfast -crf 14 -pix_fmt yuv420p10le -c:a copy clip10.mkv
fi

rm -rf "${TMPDIR:-/tmp}/dc-test-out"; : > results.log
cd "$ROOT"
run() { DC_TEST_INPUT="$HERE/$1" DC_TEST_CODECS="$2" DC_TEST_SPEED="$3" DC_TEST_MIB=$T \
          cargo test --release real_encodes -- --ignored --nocapture | sed "s/^RESULT|/RESULT|$3|$1|/" | tee -a "$HERE/results.log"; }
run clip8.mkv h265,h264,av1 balanced
run clip8.mkv h265 faster
run clip8.mkv h265 best
run clip10.mkv h265 balanced

cd "$HERE"
LIMIT=$(python3 -c "print(int($T*1024*1024))")
printf "\n%-9s %-10s %-6s %8s %8s %8s %9s\n" speed source codec time limit SSIM PSNR
grep "^RESULT" results.log | while IFS='|' read -r _ speed src codec wall size path _ _ _; do
  q=$(ffmpeg -v info -i "$path" -ss 1.0 -t 5.0 -i clip8.mkv -lavfi "[0:v]scale=1280:720:flags=bicubic,fps=60,setpts=PTS-STARTPTS,format=yuv420p,split[a1][a2];[1:v]fps=60,setpts=PTS-STARTPTS,format=yuv420p,split[b1][b2];[a1][b1]ssim;[a2][b2]psnr" -f null - 2>&1)
  ssim=$(echo "$q" | grep -oE "All:[0-9.]+" | cut -d: -f2); psnr=$(echo "$q" | grep -oE "average:[0-9.inf]+" | cut -d: -f2)
  printf "%-9s %-10s %-6s %7ss %7s%% %8s %9s\n" "$speed" "$src" "$codec" "$wall" "$(python3 -c "print(round($size/$LIMIT*100,1))")" "$ssim" "$psnr"
done
