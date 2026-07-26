#!/bin/sh
# Record exactly what spot sends your terminal on reattach.
#
# For when a reattach looks wrong and "it went black" is all we have. This
# captures the raw byte stream and reports what it contains, which separates the
# two possible faults:
#
#   - spot sent no repaint trigger  -> spot's bug
#   - spot sent one and the app did not repaint  -> the app's, or its config
#
#   ./scripts/capture-reattach.sh <session>
#
# Detach as usual when you have looked at the screen; the report prints after.

set -u

[ $# -ge 1 ] || { echo "usage: $0 <session>" >&2; exit 1; }
SESSION="$1"
LOG="${LOG:-/tmp/spot-reattach-$SESSION.raw}"

here="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$here/dist/spot" ]; then SPOT="$here/dist/spot"
elif command -v spot >/dev/null 2>&1; then SPOT="$(command -v spot)"
else echo "error: no spot binary" >&2; exit 1
fi
command -v script >/dev/null 2>&1 || { echo "error: util-linux 'script' is required" >&2; exit 1; }

"$SPOT" ls | grep -q "[[:space:]]$SESSION[[:space:]]" || {
  printf "error: no session '%s' — run '%s ls' to see what is running\n" "$SESSION" "$SPOT" >&2; exit 1; }

printf '\033[1mCapturing reattach to "%s"\033[0m\n' "$SESSION"
printf '\033[90mRaw stream -> %s\nDetach normally when you have judged the screen.\033[0m\n\n' "$LOG"
sleep 1

rm -f "$LOG"
# -q quiet, -f flush so the log survives an abrupt end.
script -q -f -c "'$SPOT' fetch '$SESSION'" "$LOG" >/dev/null 2>&1 || true

echo
printf '\033[1m---- what spot sent ----\033[0m\n'
if [ ! -s "$LOG" ]; then
  echo "  (nothing captured — did the reattach fail?)"
  exit 1
fi

bytes=$(wc -c <"$LOG" | tr -d ' ')
printf '  total bytes            : %s\n' "$bytes"

count() { printf '  %-22s : %s\n' "$1" "$(grep -c "$2" "$LOG" 2>/dev/null || echo 0)"; }
# Enter/leave alternate screen, and the full-repaint triggers.
printf '  enter alt screen (1049h): %s\n' "$(grep -ac '\[?1049h' "$LOG" 2>/dev/null || echo 0)"
printf '  leave alt screen (1049l): %s\n' "$(grep -ac '\[?1049l' "$LOG" 2>/dev/null || echo 0)"
printf '  clear screen (2J)       : %s\n' "$(grep -ac '\[2J' "$LOG" 2>/dev/null || echo 0)"
printf '  cursor home (H)         : %s\n' "$(grep -ac '\[H' "$LOG" 2>/dev/null || echo 0)"

# Printable payload is the thing that decides it: a repaint carries text.
printable=$(LC_ALL=C tr -cd '[:print:]' <"$LOG" | LC_ALL=C sed 's/\[[0-9;?]*[A-Za-z]//g' | wc -c | tr -d ' ')
printf '  printable text bytes    : %s\n' "$printable"

echo
if [ "$printable" -lt 200 ]; then
  printf '\033[31mVERDICT: almost no text was sent.\033[0m\n'
  echo "  The app did not repaint. spot restored modes and asked for a repaint,"
  echo "  and nothing came back — so the question is why the app ignored it."
  echo "  Worth trying: does the same happen with  nvim -u NONE  ?"
else
  printf '\033[32mVERDICT: a full repaint was sent (%s bytes of text).\033[0m\n' "$printable"
  echo "  If the screen still looked wrong, the bytes are right but their effect"
  echo "  was not — a terminal or TERM mismatch rather than a missing repaint."
fi
echo
printf '\033[90mSend %s along with: nvim --version | head -1, echo $TERM,\nyour terminal emulator, and whether zellij/tmux was involved.\033[0m\n' "$LOG"
