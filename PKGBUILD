# Arch Linux / Omarchy package for Simple VM Manager.
#
# Build and install from a clone of the repository (no download needed):
#     git clone https://github.com/jpaferreira-git/simple-vm-manager.git
#     cd simple-vm-manager
#     makepkg -si
#
# Remove it again with:  sudo pacman -R simple-vm-manager

pkgname=simple-vm-manager
pkgver=1.10.3
pkgrel=1
pkgdesc="Simple desktop console for XenServer / XCP-ng pools: browse servers and VMs, use their consoles, start, shut down, reboot, suspend"
arch=('x86_64' 'aarch64')
url="https://github.com/jpaferreira-git/simple-vm-manager"
license=('MIT' 'OFL-1.1')
depends=(
  'gcc-libs' 'glibc' 'openssl'
  'libxkbcommon' 'wayland' 'libx11' 'libxcursor' 'libxrandr' 'libxi'
  'vulkan-icd-loader' 'mesa'
  'hicolor-icon-theme'
)
makedepends=('cargo' 'pkgconf')
options=('!lto' '!debug')
source=()
sha256sums=()

# Builds the checkout this PKGBUILD sits in.
_src() { echo "$startdir"; }

prepare() {
  cd "$(_src)"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$(_src)"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR="$srcdir/target"
  cargo build --frozen --release
}

package() {
  cd "$(_src)"
  install -Dm755 "$srcdir/target/release/simple-vm-manager" "$pkgdir/usr/bin/simple-vm-manager"
  install -Dm644 packaging/simple-vm-manager.desktop "$pkgdir/usr/share/applications/simple-vm-manager.desktop"
  install -Dm644 packaging/simple-vm-manager.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/simple-vm-manager.svg"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 assets/fonts/LICENSE-Inter.txt "$pkgdir/usr/share/licenses/$pkgname/LICENSE-Inter.txt"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
