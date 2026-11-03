#!/usr/bin/env bash
# macOS only, CI experiment: starts a debug Gantry on a zsh pane, presses Cmd+Left / Cmd+Right through System Events
# and checks, from files the shell creates, where the cursor went. Needs the Accessibility permission for osascript.
# Usage: scripts/mac-keys-check.sh <gantry binary> <work dir>
set -u
BIN=$1
WORK=$2
mkdir -p "$WORK"
cd "$WORK"
rm -f X.txt A.txt

# A throwaway HOME for Gantry only, with the setup wizard already answered (it would cover the pane).
H="$WORK/home"
for dir in "$H/Library/Application Support/gantry" "$H/.config/gantry"; do
  mkdir -p "$dir"
  echo '{"setupDone": true}' > "$dir/settings.json"
done

export GANTRY_PROFILE=mackeys
HOME="$H" "$BIN" --cwd "$WORK" -- /bin/zsh -f > gantry.log 2>&1 &
PID=$!
sleep 15

keys() { osascript -e "tell application \"System Events\"
$1
end tell"; }

if ! keys 'set frontmost of first process whose unix id is '"$PID"' to true'; then
  echo "System Events refused (Accessibility permission missing?)"
  kill "$PID" 2>/dev/null
  exit 2
fi
sleep 2

# Cmd+Left: "X.txt", home, "touch ", Enter -> `touch X.txt` only if the cursor really went to the line start.
keys 'keystroke "X.txt"
key code 123 using command down
keystroke "touch "
key code 36'
sleep 2
# Cmd+Right: "touch A", Cmd+Left, Cmd+Right, ".txt", Enter -> `touch A.txt` only if the cursor came back to the end.
keys 'keystroke "touch A"
key code 123 using command down
key code 124 using command down
keystroke ".txt"
key code 36'
sleep 2

screencapture -x screen.png || true
kill "$PID" 2>/dev/null
pkill -f "gantry daemon" 2>/dev/null || true

status=0
[ -f X.txt ] && echo "Cmd+Left: ok" || { echo "Cmd+Left: X.txt missing"; status=1; }
[ -f A.txt ] && echo "Cmd+Right: ok" || { echo "Cmd+Right: A.txt missing"; status=1; }
exit $status
