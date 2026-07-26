#!/bin/sh
# Manual reattach matrix (RFC-0002 §13), made runnable.
#
# The parts of spot that matter most cannot be asserted from a test harness:
# whether the screen *looks right* after a reattach is a judgement call. So this
# drives the mechanical half — creating, detaching, killing, reattaching,
# checking session state — and asks you to judge only the visual half.
#
#   ./scripts/manual-reattach-test.sh            # nvim (the default)
#   APP=htop ./scripts/manual-reattach-test.sh   # or any other full-screen app
#   APP='less /etc/services' ./scripts/manual-reattach-test.sh
#
# Runs in ONE terminal: the script puts the session in the foreground and gets
# control back when you detach or when it kills the client for you.

set -u

APP="${APP:-nvim}"
APP_BIN="${APP%% *}"
SESSION="rtest$$"
SCRATCH="/tmp/spot-rtest-$$"
PIDFILE="$SCRATCH/client.pid"
PASS=0
FAIL=0

# --- locate spot -------------------------------------------------------------
here="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$here/dist/spot" ]; then
  SPOT="$here/dist/spot"
elif command -v spot >/dev/null 2>&1; then
  SPOT="$(command -v spot)"
else
  echo "error: no spot binary (looked in $here/dist/spot and PATH)" >&2
  exit 1
fi
command -v "$APP_BIN" >/dev/null 2>&1 || { echo "error: $APP_BIN not installed" >&2; exit 1; }

# `stay` must be reachable from inside the session, which is the whole point of
# the symlink the installer makes. Fall back to a scratch one so the test can
# run against a plain build.
STAYDIR="$SCRATCH/bin"
mkdir -p "$STAYDIR"
ln -sf "$SPOT" "$STAYDIR/stay"
PATH="$STAYDIR:$PATH"
export PATH

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
dim()  { printf '\033[90m%s\033[0m\n' "$1"; }

ask() { # ask "question"
  printf '\n\033[1;33m?\033[0m %s \033[90m[y/n/s=skip]\033[0m ' "$1"
  read -r a </dev/tty
  case "$a" in
    y|Y) PASS=$((PASS+1)); printf '  \033[32m✓ pass\033[0m\n' ;;
    s|S) printf '  \033[90m– skipped\033[0m\n' ;;
    *)   FAIL=$((FAIL+1)); printf '  \033[31m✗ FAIL\033[0m — note what you saw\n' ;;
  esac
}

pause() { printf '\n\033[90m%s\033[0m' "press enter to continue "; read -r _ </dev/tty; }

state() { "$SPOT" ls 2>/dev/null | awk -v s="$SESSION" '$3==s {print $2}'; }

check_state() { # check_state expected label
  got="$(state)"
  if [ "$got" = "$1" ]; then
    printf '  \033[32m✓\033[0m %s (spot ls: %s)\n' "$2" "${got:-gone}"
    PASS=$((PASS+1))
  else
    printf '  \033[31m✗\033[0m %s — expected %s, got %s\n' "$2" "$1" "${got:-gone}"
    FAIL=$((FAIL+1))
  fi
}

cleanup() {
  "$SPOT" drop "$SESSION" --force >/dev/null 2>&1
  rm -rf "$SCRATCH"
}
trap cleanup EXIT INT TERM

# --- a file whose every line is identifiable, so a stale or torn screen shows --
mkdir -p "$SCRATCH"
FILE="$SCRATCH/sample.txt"
i=1
while [ "$i" -le 300 ]; do
  printf 'line %03d ........................................ %03d\n' "$i" "$i" >>"$FILE"
  i=$((i+1))
done

clear
bold "spot manual reattach test — $APP"
dim  "binary:  $SPOT"
dim  "session: $SESSION   scratch: $SCRATCH"
echo
dim  "Judge the SCREEN; the script checks session state itself."
pause

# =============================================================================
bold "
1/5  Create, then detach from inside
"
cat <<EOF
  You will land in $APP_BIN.

    - Move around so the screen is definitely painted (G, then gg in nvim).
    - Then detach from INSIDE, which for a full-screen app means asking it to
      run the command for you — there is no shell to type into:

          nvim:  :!stay
          htop:  quit is the only way out, so use  q  and skip this one
          less:  !stay

  You should return here with "Detached ... Spot will stay!".
EOF
pause
"$SPOT" "$SESSION" -- $APP "$FILE"
echo
check_state detached "session survived the detach"
ask "Did the screen look correct while in $APP_BIN, and did the detach report cleanly?"

# =============================================================================
bold "
2/5  Reattach — the case that decides everything
"
cat <<EOF
  Reattaching now. What should happen: a clean, complete repaint of $APP_BIN.

  What to look for:
    - no black screen
    - no stale/duplicated rows from a previous frame
    - no torn or half-drawn lines
    - cursor where you left it

  Detach again the same way when you have looked ( :!stay ).
EOF
pause
"$SPOT" fetch "$SESSION"
echo
check_state detached "still alive after the second detach"
ask "Did it repaint cleanly and completely?"

# =============================================================================
bold "
3/5  Simulated network drop (SIGKILL the client, no cleanup)
"
cat <<EOF
  This is the case spot exists for: the client dies without any chance to tidy
  up, exactly as when WiFi drops or a laptop lid closes.

  You will be reattached, and after 15 seconds the client is killed from
  underneath you. Your shell prompt should come back abruptly.
EOF
pause
( sleep 15; [ -f "$PIDFILE" ] && kill -9 "$(cat "$PIDFILE")" 2>/dev/null ) &
KILLER=$!
sh -c "echo \$\$ > '$PIDFILE'; exec '$SPOT' fetch '$SESSION'"
wait "$KILLER" 2>/dev/null
echo
check_state detached "child survived a SIGKILLed client"
ask "Did the session survive, and is your terminal still usable (no stuck mouse/alt-screen)?"

# =============================================================================
bold "
4/5  Reattach at a different size
"
cat <<EOF
  RESIZE THIS TERMINAL WINDOW NOW, while the session is detached — make it
  noticeably narrower or shorter.

  Then we reattach. $APP_BIN should come back laid out for the NEW size, not the
  old one. Detach again with :!stay when done.
EOF
pause
"$SPOT" fetch "$SESSION"
echo
check_state detached "survived the resized reattach"
ask "Did it come back correctly laid out for the new window size?"

# =============================================================================
bold "
5/5  Quit the app — 'ended' must not look like 'detached'
"
cat <<EOF
  Reattaching one last time. Quit $APP_BIN properly this time:

      nvim:  :q!      htop:  q      less:  q

  Expect:  🦴 Session '$SESSION' ended. Spot is off duty.
EOF
pause
"$SPOT" fetch "$SESSION"
echo
if [ -z "$(state)" ]; then
  printf '  \033[32m✓\033[0m session is gone from spot ls\n'; PASS=$((PASS+1))
else
  printf '  \033[31m✗\033[0m session still listed after the app exited\n'; FAIL=$((FAIL+1))
fi
ask "Did you get the 'ended' message (not 'detached')?"

# =============================================================================
echo
bold "----------------------------------------"
printf 'passed: \033[32m%s\033[0m   failed: \033[31m%s\033[0m\n' "$PASS" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  echo
  dim "For anything that failed, the useful details are:"
  dim "  - which step, and what the screen actually showed"
  dim "  - your \$TERM, terminal emulator, and whether it was inside zellij/tmux"
fi
bold "----------------------------------------"
