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
rm -f -- "$prefix/share/termie/fonts/MapleMono-LICENSE.txt" "$prefix/share/termie/fonts/MapleMono-NF-Bold.ttf" "$prefix/share/termie/fonts/MapleMono-NF-BoldItalic.ttf" "$prefix/share/termie/fonts/MapleMono-NF-Italic.ttf" "$prefix/share/termie/fonts/MapleMono-NF-Regular.ttf" "$prefix/share/termie/fonts/OFL.txt" "$prefix/share/termie/fonts/SourceSans3-LICENSE.txt" "$prefix/share/termie/fonts/SourceSans3-Medium.ttf" "$prefix/share/termie/fonts/SourceSans3-Regular.ttf"
rm -f -- "$prefix/share/termie/archive-install"
rmdir -- "$prefix/share/termie/fonts" "$prefix/share/termie" 2>/dev/null || true

config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
for list in "$config_home"/xdg-terminals.list "$config_home"/*-xdg-terminals.list; do
  [ -f "$list" ] || continue
  temporary="$list.termie-tmp"
  awk '$0 !~ /^[[:space:]]*termie[.]desktop[[:space:]]*$/' "$list" > "$temporary"
  if [ -s "$temporary" ]; then
    mv -f -- "$temporary" "$list"
  else
    rm -f -- "$temporary" "$list"
  fi
done

kde_snapshot="$config_home/termie/default-terminal-kde"
if [ -f "$kde_snapshot" ] && command -v kreadconfig6 >/dev/null 2>&1 && command -v kwriteconfig6 >/dev/null 2>&1; then
  current=$(kreadconfig6 --file kdeglobals --group General --key TerminalService 2>/dev/null || true)
  if [ "$current" = termie.desktop ]; then
    snapshot=$(tr '\000' '\n' < "$kde_snapshot")
    application=$(printf '%s\n' "$snapshot" | sed -n '1p')
    service=$(printf '%s\n' "$snapshot" | sed -n '2p')
    case "$application" in
      1*) kwriteconfig6 --file kdeglobals --group General --key TerminalApplication --notify "${application#?}" || true ;;
      0*) kwriteconfig6 --file kdeglobals --group General --key TerminalApplication --notify --delete '' || true ;;
    esac
    case "$service" in
      1*) kwriteconfig6 --file kdeglobals --group General --key TerminalService --notify "${service#?}" || true ;;
      0*) kwriteconfig6 --file kdeglobals --group General --key TerminalService --notify --delete '' || true ;;
    esac
  fi
  rm -f -- "$kde_snapshot"
fi

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" >/dev/null 2>&1 || true
printf 'removed termie from %s\n' "$prefix"
