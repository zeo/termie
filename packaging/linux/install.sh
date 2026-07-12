#!/bin/sh
set -eu

prefix=${1:-"$HOME/.local"}
case "$prefix" in
  /*) ;;
  *) printf 'prefix must be an absolute path: %s\n' "$prefix" >&2; exit 2 ;;
esac

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
install -d "$prefix/bin" "$prefix/share/applications" "$prefix/share/icons/hicolor/256x256/apps" "$prefix/share/doc/termie" "$prefix/share/termie/fonts"
install -m755 "$root/bin/termie" "$prefix/bin/termie"
install -m644 "$root/share/applications/termie.desktop" "$prefix/share/applications/termie.desktop"
install -m644 "$root/share/icons/hicolor/256x256/apps/termie.png" "$prefix/share/icons/hicolor/256x256/apps/termie.png"
install -m644 "$root/share/doc/termie/"* "$prefix/share/doc/termie/"
install -m644 "$root/share/termie/fonts/"* "$prefix/share/termie/fonts/"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" >/dev/null 2>&1 || true
printf 'installed termie under %s\n' "$prefix"
