#!/bin/sh
set -eu

prefix=${1:-"$HOME/.local"}
case "$prefix" in
  /*) ;;
  *) printf 'prefix must be an absolute path: %s\n' "$prefix" >&2; exit 2 ;;
esac

rm -f -- "$prefix/bin/termie" "$prefix/share/applications/termie.desktop" "$prefix/share/icons/hicolor/256x256/apps/termie.png"
rm -f -- "$prefix/share/doc/termie/LICENSE-MIT" "$prefix/share/doc/termie/LICENSE-APACHE" "$prefix/share/doc/termie/THIRDPARTY.md" "$prefix/share/doc/termie/README.md"
rmdir -- "$prefix/share/doc/termie" 2>/dev/null || true
rm -f -- "$prefix/share/termie/fonts/MapleMono-LICENSE.txt" "$prefix/share/termie/fonts/MapleMono-NF-Bold.ttf" "$prefix/share/termie/fonts/MapleMono-NF-BoldItalic.ttf" "$prefix/share/termie/fonts/MapleMono-NF-Italic.ttf" "$prefix/share/termie/fonts/MapleMono-NF-Regular.ttf" "$prefix/share/termie/fonts/OFL.txt"
rmdir -- "$prefix/share/termie/fonts" "$prefix/share/termie" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" >/dev/null 2>&1 || true
printf 'removed termie from %s\n' "$prefix"
