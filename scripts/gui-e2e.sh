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
#
#   scripts/gui-e2e.sh term     # the windowed terminal instead (phase 3.5)
#
# `term` mode checks, in order (rows of colour drawn with printf are what
# the screendumps are checked for, so no OCR is needed):
#   T1. `compositor term` maps an 80x25 window with a focused title bar.
#   T2. `clear; printf` of an 80-column red row: it fills row 0 exactly, the
#       deferred wrap leaves no blank row after it, and a new prompt
#       follows (parser -> grid -> render -> surface, end to end).
#   T3. ^C ends a `sleep 30` in the window and the compositor survives it
#       (the grab's console ^C must not reach it); a green row follows.
#   T4. `vi` takes the alternate screen (the red row is gone) and `:q`
#       brings the primary one back.
#   T5. `exit` in the window: ash exits, term exits, the window goes away.
#   T6. Ctrl+Alt+Backspace; the console comes back and typing works.
#
#   scripts/gui-e2e.sh text     # proportional text (docs/gui/text-plan.md)
#
# `text` mode runs `compositor /mnt/bin/textdemo`, which logs the box
# `measure` gave every piece of text it drew (window coordinates), and
# checks those boxes against the ink on a screendump:
#   X1. The window is up and textdemo loaded the TrueType fonts (not the
#       bitmap fallback).
#   X2. For every box: ink at its left edge and at its right edge (within
#       a side bearing), none in the 10 px right of it — measure is where the text
#       starts and ends. And mono wider than sans, bold wider than regular.
#   X3. Esc quits textdemo and its window goes; Ctrl+Alt+Backspace gives
#       the console back.
# Timings and the glyph cache's counters are printed from the log.
# Exit status: number of failed checks.
set -u
MODE=${1:-demo}
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
has() { # has <png> <r,g,b> <x0> <y0> <x1> <y1>  -> yes/no
    python3 -c "
from PIL import Image
im = Image.open('$1').convert('RGB'); want = tuple(int(v) for v in '$2'.split(','))
print('yes' if any(im.getpixel((x, y)) == want for y in range($4, $6) for x in range($3, $5)) else 'no')"
}
shot() { $Q screendump "$OUT/$1.png" >/dev/null; echo "$OUT/$1.png"; }

BG=32,48,64          # gui::compositor::BACKGROUND
FOCUSED=80,120,176   # TITLE_FOCUSED

$Q stop >/dev/null 2>&1
QEMU_DEBUG_SMP=${QEMU_DEBUG_SMP:-4} $Q start >/dev/null 2>&1 || { echo "start failed"; exit 99; }
$Q wait-for "/ #" 120 >/dev/null || { echo "no shell prompt"; exit 99; }

if [ "$MODE" = term ]; then
    RED=224,108,117; GREEN=152,195,121; BLACK=0,0,0
    # Content origin (40, 40 + title bar); cells are read from term's log.
    X0=40; Y0=60
    $Q send "compositor term" && $Q enter
    $Q wait-for "term: 80x25 cells" 30 >/dev/null || bad "T1 term never started"
    cell=$(grep -ao "term: 80x25 cells of [0-9]*x[0-9]*" "$STATE/serial.log" | tail -1 | grep -o "[0-9]*x[0-9]*$")
    CW=${cell%x*}; CH=${cell#*x}; CW=${CW:-9}; CH=${CH:-20}
    X1=$((X0 + 80 * CW)); Y1=$((Y0 + 25 * CH))
    sleep 2
    s=$(shot t1)
    [ "$(px "$s" 45 45)" = "$FOCUSED" ] && ok "T1 focused term window at (40,40)" || bad "T1 title bar: $(px "$s" 45 45)"

    $Q send "clear; printf '\\033[41m%80s\\033[0m\\n' ''" && $Q enter; sleep 2
    s=$(shot t2)
    mid=$((Y0 + CH / 2))
    [ "$(px "$s" $((X0 + 2)) $mid)" = "$RED" ] && [ "$(px "$s" $((X1 - 2)) $mid)" = "$RED" ] \
        && ok "T2 red row spans row 0" || bad "T2 row 0: $(px "$s" $((X0 + 2)) $mid) .. $(px "$s" $((X1 - 2)) $mid)"
    [ "$(px "$s" $((X1 + 2)) $mid)" != "$RED" ] && ok "T2 nothing past column 80" || bad "T2 red outside the window"
    [ "$(has "$s" "$RED" $X0 $((Y0 + CH)) $X1 $Y1)" = no ] && ok "T2 only one red row" || bad "T2 red below row 0"
    [ "$(has "$s" "$BLACK" $X0 $((Y0 + CH)) $((X0 + 4 * CW)) $((Y0 + 2 * CH)))" = yes ] \
        && [ "$(px "$s" $((X0 + 4 * CW + 2)) $((Y0 + CH + CH / 2)))" != "$BLACK" ] \
        && ok "T2 prompt right below (deferred wrap)" || bad "T2 no prompt on row 1"

    $Q send "sleep 30" && $Q enter; sleep 1; $Q key ctrl-c; sleep 1
    $Q send "printf '\\033[42m%10s\\033[0m\\n' ''" && $Q enter; sleep 2
    s=$(shot t3)
    [ "$(has "$s" "$GREEN" $X0 $Y0 $X1 $Y1)" = yes ] && ok "T3 ^C ended sleep; the shell answers" || bad "T3 no green row"
    [ "$(px "$s" 45 45)" = "$FOCUSED" ] && ok "T3 compositor survived ^C" || bad "T3 window gone: $(px "$s" 45 45)"
    grep -aq "Killed PID [0-9]* (compositor)" "$STATE/serial.log" && bad "T3 compositor killed"

    $Q send "vi /tmp/e2e.txt" && $Q enter; sleep 2
    s=$(shot t4)
    [ "$(has "$s" "$RED" $X0 $Y0 $X1 $Y1)" = no ] && ok "T4 vi on the alternate screen" || bad "T4 red row still visible in vi"
    $Q send ":q" && $Q enter; sleep 2
    s=$(shot t5)
    [ "$(px "$s" $((X0 + 2)) $mid)" = "$RED" ] && [ "$(has "$s" "$GREEN" $X0 $Y0 $X1 $Y1)" = yes ] \
        && ok "T4 :q restored the primary screen" || bad "T4 primary screen not restored"

    $Q send "exit" && $Q enter
    $Q wait-for "term: shell gone" 15 >/dev/null && ok "T5 exit: term saw the shell go" || bad "T5 term did not notice"
    sleep 1
    s=$(shot t6)
    [ "$(px "$s" 45 45)" = "$BG" ] && ok "T5 window gone" || bad "T5 window still there: $(px "$s" 45 45)"

    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "T6 console and keyboard back" || bad "T6 typing does not reach ash"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e term: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

if [ "$MODE" = text ]; then
    X0=40; Y0=60           # content origin of the first window
    TBG=30,33,39           # textdemo's background
    $Q send "compositor /mnt/bin/textdemo" && $Q enter
    $Q wait-for "textdemo: ready" 120 >/dev/null || bad "X1 textdemo never got ready"
    sleep 2
    s=$(shot x1)
    [ "$(px "$s" 45 45)" = "$FOCUSED" ] && ok "X1 focused textdemo window at (40,40)" || bad "X1 title bar: $(px "$s" 45 45)"
    grep -aq "textdemo: fonts loaded" "$STATE/serial.log" && ok "X1 TrueType fonts loaded" \
        || bad "X1 $(grep -a 'textdemo: no fonts' "$STATE/serial.log" | tail -1)"

    grep -a "textdemo: box " "$STATE/serial.log" | sed 's/.*textdemo: box //' > "$OUT/boxes"
    r=$(python3 - "$s" "$OUT/boxes" $X0 $Y0 $TBG <<'PY'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert('RGB')
x0, y0 = int(sys.argv[3]), int(sys.argv[4])
bg = tuple(int(v) for v in sys.argv[5].split(','))
def ink(xa, xb, ya, yb):
    return any(im.getpixel((x, y)) != bg for y in range(ya, yb) for x in range(xa, xb))
boxes, fails = {}, 0
for line in open(sys.argv[2]):
    name, x, y, w, h = line.split()
    x, y, w, h = int(x) + x0, int(y) + y0, int(w), int(h)
    boxes[name] = w
    errs = []
    # A glyph's side bearing leaves a gap between the advance box and its
    # ink that grows with the size (a 72 px 'l' ends ~7 px short): allow a
    # tenth of the line height.
    sb = max(3, h // 10)
    if not ink(x - 1, x + sb, y, y + h): errs.append('no ink at the left edge')
    if not ink(x + w - sb, x + w + 2, y, y + h): errs.append('no ink at the right edge')
    if ink(x + w + 2, x + w + 12, y, y + h): errs.append('ink right of the box')
    if errs:
        fails += 1
        print('FAIL X2 %s (%d,%d %dx%d): %s' % (name, x, y, w, h, ', '.join(errs)))
if len(boxes) < 15:
    fails += 1; print('FAIL X2 only %d boxes logged' % len(boxes))
if not (boxes.get('mono', 0) > boxes.get('sans', 0) and boxes.get('sansbold', 0) > boxes.get('sans', 0)):
    fails += 1; print('FAIL X2 widths sans/sansbold/mono: %s' % [boxes.get(k) for k in ('sans', 'sansbold', 'mono')])
print('checked %d boxes' % len(boxes))
sys.exit(fails)
PY
)
    n=$?
    echo "$r" | grep FAIL | sed 's/^/  /'
    fails=$((fails + n))
    [ $n = 0 ] && ok "X2 ink matches measure ($(echo "$r" | tail -1))"
    grep -a "textdemo: \(fonts\|first\|latin\)" "$STATE/serial.log" | sed 's/^.fb. /       /'

    $Q key esc
    $Q wait-for "textdemo: bye" 15 >/dev/null && ok "X3 Esc quits" || bad "X3 textdemo did not quit"
    sleep 1
    s=$(shot x3)
    [ "$(px "$s" 45 45)" = "$BG" ] && ok "X3 window gone" || bad "X3 window still there: $(px "$s" 45 45)"
    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "X3 console and keyboard back" || bad "X3 typing does not reach ash"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e text: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

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
