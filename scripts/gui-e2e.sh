#!/usr/bin/env bash
# End-to-end test of the compositor (phase 2.5 of docs/gui/gui-plan.md),
# driven through QEMU's monitor and checked on screendumps pixel by pixel.
#
#   scripts/gui-e2e.sh          # boots its own headless QEMU (qemu-debug.sh)
#
# Environment is passed through to qemu-debug.sh, so the USB input path is
#   QEMU_USB_KBD=1 QEMU_USB_MOUSE=1 QEMU_DEBUG_NO_PS2=1 scripts/gui-e2e.sh
#
# Checks, in order:
#   1. `compositor gui_demo` puts a 320x200 window at (40,40) with a focused
#      title bar, and the software cursor at the screen's centre.
#   2. Moving the mouse lands the cursor exactly where it was sent (1:1).
#   3. Dragging the title bar by (+300,+200) moves the window there, and
#      where it was is background again.
#   4. A right click and two keys in the window reach gui_demo (its stdout).
#   5. The keys did not reach ash (the keyboard was grabbed).
#   6. Ctrl+Alt+Backspace quits; gui_demo sees EOF; the console comes back
#      and typing works.
# Exit status: number of failed checks.
set -u
cd "$(dirname "$0")/.."
Q=scripts/qemu-debug.sh
STATE=${QEMU_DEBUG_STATE_DIR:-/tmp/qemu-debug-rust_so_kernel}
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT
fails=0
ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; fails=$((fails + 1)); }

px() { # px <png> <x> <y>  -> "r,g,b"
    python3 -c "
from PIL import Image
im = Image.open('$1').convert('RGB'); print('%d,%d,%d' % im.getpixel(($2, $3)))"
}
cursor() { # cursor <png> -> "x y" of the cursor tip (black with white below-right)
    python3 -c "
from PIL import Image
im = Image.open('$1').convert('RGB'); W, H = im.size
for y in range(H - 2):
    for x in range(W - 2):
        if im.getpixel((x, y)) == (0, 0, 0) and im.getpixel((x + 1, y + 2)) == (255, 255, 255):
            print(x, y); raise SystemExit
print('none')"
}
shot() { $Q screendump "$OUT/$1.png" >/dev/null; echo "$OUT/$1.png"; }

BG=32,48,64          # gui::compositor::BACKGROUND
FOCUSED=80,120,176   # TITLE_FOCUSED

$Q stop >/dev/null 2>&1
QEMU_DEBUG_SMP=${QEMU_DEBUG_SMP:-4} $Q start >/dev/null 2>&1 || { echo "start failed"; exit 99; }
$Q wait-for "/ #" 120 >/dev/null || { echo "no shell prompt"; exit 99; }

$Q send "compositor gui_demo" && $Q enter
$Q wait-for "gui_demo: focus in" 30 >/dev/null || bad "gui_demo never got focus"
sleep 1

s=$(shot one)
[ "$(px "$s" 45 45)" = "$FOCUSED" ] && ok "1 focused title bar at (40,40)" || bad "1 title bar: $(px "$s" 45 45)"
c=$(px "$s" 100 150); [ "$c" != "$BG" ] && ok "1 window content at (100,150)" || bad "1 no content at (100,150)"
[ "$(cursor "$s")" = "640 400" ] && ok "1 cursor at the centre" || bad "1 cursor at $(cursor "$s")"

$Q mouse-move -440 -350; sleep 0.5
s=$(shot two)
[ "$(cursor "$s")" = "200 50" ] && ok "2 cursor moved 1:1 to (200,50)" || bad "2 cursor at $(cursor "$s")"

$Q mouse-button 1; $Q mouse-move 150 100; $Q mouse-move 150 100; $Q mouse-button 0; sleep 0.5
s=$(shot three)
[ "$(px "$s" 345 245)" = "$FOCUSED" ] && ok "3 title bar dragged to (340,240)" || bad "3 title at (345,245): $(px "$s" 345 245)"
[ "$(px "$s" 100 150)" = "$BG" ] && ok "3 old place is background" || bad "3 old place: $(px "$s" 100 150)"

$Q mouse-move 0 100; $Q mouse-button 2; $Q mouse-button 0
$Q send "ab"; sleep 1
log=$(grep -a "gui_demo:" "$STATE/serial.log")
grep -q "button 0x111 down" <<<"$log" && ok "4 right click reached the window" || bad "4 no button event"
grep -q "key 30 down" <<<"$log" && grep -q "key 48 up" <<<"$log" && ok "4 keys a,b reached the window" || bad "4 no key events"

$Q key ctrl-alt-backspace
$Q wait-for "gui_demo: compositor gone" 15 >/dev/null && ok "6 quit; gui_demo saw EOF" || bad "6 gui_demo not told"
sleep 1
$Q send "echo console-is-back" && $Q enter
$Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "6 console and keyboard back" || bad "6 typing does not reach ash"
grep -aq "^\[fb\] / # abecho" "$STATE/serial.log" && bad "5 grabbed keys leaked into ash" || ok "5 grabbed keys did not reach ash"
grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"

$Q stop >/dev/null 2>&1
echo "gui-e2e: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
exit $fails
