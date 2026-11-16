#!/usr/bin/env bash
# Build Simple VM Manager and install it for the current user (no root needed):
#   ~/.local/bin/simple-vm-manager, a launcher entry and an icon.
# On Arch / Omarchy, `makepkg -si` (see PKGBUILD) installs it system-wide instead.
#
#   ./install.sh            build and install
#   ./install.sh --uninstall remove it again
set -euo pipefail

cd "$(dirname "$0")"
BIN="$HOME/.local/bin"
APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICONS="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"

# for a short while the app was called XenCraft: those files go too
OLD=("$BIN/xencraft" "$APPS/xencraft.desktop" "$ICONS/xencraft.svg")

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$BIN/simple-vm-manager" "$APPS/simple-vm-manager.desktop" "$ICONS/simple-vm-manager.svg" "${OLD[@]}"
  echo "Removed Simple VM Manager."
  exit 0
fi

if [[ -x ./simple-vm-manager && ! -f Cargo.toml ]]; then
  SRC=./simple-vm-manager            # prebuilt release archive: nothing to build
else
  if ! command -v cargo >/dev/null 2>&1; then
    echo "Rust isn't installed. On Arch / Omarchy:  sudo pacman -S --needed rust pkgconf openssl"
    exit 1
  fi
  cargo build --release --locked
  SRC=target/release/simple-vm-manager
fi

install -Dm755 "$SRC" "$BIN/simple-vm-manager"
install -Dm644 packaging/simple-vm-manager.desktop "$APPS/simple-vm-manager.desktop"
install -Dm644 packaging/simple-vm-manager.svg "$ICONS/simple-vm-manager.svg"
rm -f "${OLD[@]}"
# point the launcher at the user copy, in case ~/.local/bin isn't on PATH
sed -i "s|^Exec=.*|Exec=$BIN/simple-vm-manager|" "$APPS/simple-vm-manager.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q "${ICONS%/scalable/apps}" || true

echo "Installed. Start it from your app launcher (Super + Space on Omarchy) or run: simple-vm-manager"
