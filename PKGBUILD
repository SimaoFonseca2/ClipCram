# Builds and installs ClipCram from this checkout, including uncommitted changes:
#
#   makepkg -si
#
# makepkg normally keeps its work files in a folder called "src" next to the PKGBUILD,
# which here is ClipCram's own source code (and `makepkg -c` would delete it). So this
# PKGBUILD sends makepkg's work files to .makepkg/ instead.

pkgname=clipcram
_root="$startdir"
if [[ -n ${srcdir:-} && $srcdir -ef "$_root/src" ]]; then
  # Safety net in case a future makepkg picks its work folder before reading this file.
  echo "==> ERROR: makepkg would use ClipCram's src/ folder as its work folder; aborting." >&2
  exit 1
fi
if [[ ${BUILDDIR:-$startdir} -ef "$startdir" ]]; then
  BUILDDIR="$startdir/.makepkg"
fi

# The version comes from git (for example 1.0.0.r3.gabc1234: three commits after v1.0.0).
# It's worked out here rather than in a pkgver() function, which would make makepkg
# rewrite this file on every build.
pkgver="$(
  cd "$_root" &&
    { git describe --long --tags --abbrev=7 2>/dev/null | sed 's/^v//;s/\([^-]*-g\)/r\1/;s/-/./g' | grep . ||
      printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"; }
)"
pkgrel=1
pkgdesc="Trim and squeeze videos under Discord's upload limit"
arch=('x86_64' 'aarch64')
url="$(git -C "$_root" remote get-url origin 2>/dev/null |
  sed -E 's#^(git@github\.com:|ssh://git@github\.com/)#https://github.com/#; s#\.git$##')"
license=('MIT')
depends=(
  'alsa-lib'
  'ffmpeg'
  'gcc-libs'
  'glibc'
  'hicolor-icon-theme'
  'libglvnd'
  'libx11'
  'libxcursor'
  'libxi'
  'libxkbcommon'
  'libxrandr'
  'wayland'
)
makedepends=('cargo' 'git')
optdepends=(
  'xdg-desktop-portal: native file dialogs'
  'zenity: file dialogs when no desktop portal is running'
)
# Cargo.toml already enables LTO and stripping for release builds.
options=('!lto' '!debug')

# Kept next to makepkg's work folder (and outside it, so `makepkg -c` doesn't throw away
# the compiled dependencies) and separate from your normal `cargo build` output.
_target() { echo "$srcdir/../target"; }

build() {
  cd "$_root"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR="$(_target)"
  cargo build --release --locked
}

check() {
  cd "$_root"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR="$(_target)"
  cargo test --release --locked
}

package() {
  cd "$_root"
  install -Dm755 "$(_target)/release/$pkgname" "$pkgdir/usr/bin/$pkgname"
  install -Dm644 "$pkgname.desktop" "$pkgdir/usr/share/applications/$pkgname.desktop"
  install -Dm644 "icons/$pkgname.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/$pkgname.svg"
  for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "icons/$pkgname-$size.png" \
      "$pkgdir/usr/share/icons/hicolor/${size}x${size}/apps/$pkgname.png"
  done
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
