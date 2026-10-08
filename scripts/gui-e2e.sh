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
#   4. A right click, two keys and the wheel (a notch each way) in the
#      window reach gui_demo (its stdout).
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
#
#   scripts/gui-e2e.sh wm       # window management (phase 4)
#
# `wm` mode runs `compositor` with no arguments (a session: it starts
# `panel`), finds the panel's buttons from its log line, drives the mouse
# with `goto` (moves, then corrects from the cursor it finds on a
# screendump) and checks:
#   W1. The panel takes the last strip of the screen and its launcher
#       starts `term`, in the list and focused.
#   W2. Dragging term's bottom-right corner by (-200,-100): one resize,
#       whole cells, and `stty size` in the window says so.
#   W3. Maximize: the frame fills the work area and the panel stays
#       visible; `stty size` grows. Restore goes back.
#   W5. With `cpumon` on top, term's button in the panel raises and
#       focuses it.
#   W4. cpumon's close button ends it; term's ends term (ash hangs up).
#   W6. `fire` (constanos_gfx.h, fixed size) has no maximize button, and
#       dragging just past its right edge does not change it.
#   W7. The start menu (a popup): it shows above the strip, Escape and a
#       click outside close it, and a theme picked in it (9x) changes the
#       taskbar (the strip is the compositor's), then back to Luna.
#   scripts/gui-e2e.sh std      # a Rust std client (gui-client crate)
#
# `std` mode runs `compositor /mnt/bin/hello-window` (Rust std on musl, the
# window through `gui_client`), which prints every event it gets:
#   S1. A focused 320x200 window at (40,40) showing hello-window's
#       gradient, pixel exact.
#   S2. Pointer motion arrives in window coordinates, 1:1, and the square
#       is drawn under the pointer.
#   S3. A left click and two keys reach it.
#   S4. Esc ends it with exit(0) and the window goes; the console comes back.
#   scripts/gui-e2e.sh ui       # the `ui` widgets and the semantic tree (stage 7)
#
# `ui` mode runs `compositor /mnt/bin/ui-demo term` and reads ui-demo's
# semantic tree with `gui-tree` typed in term (term is dragged below
# ui-demo first; clicking term's title bar, then ui-demo's, moves the
# keyboard between them):
#   U1. The tree has the window, the button, the field, the places and a
#       10000-row list of which only the shown rows are nodes.
#   U2. Tab moves the focus to the field; typed text and Enter reach it
#       (Submitted, the field's value in the tree).
#   U3. Tab to the list, End: the last row is selected and in view, still
#       only the shown rows; "d" finds "date 00003"; the status label says
#       so; the selected row is the theme's selection colour on screen.
#   U4. A click on the button (its bounds from the tree) is a click; a
#       double click on a row opens it; the wheel scrolls the list.
#   U5. Esc ends ui-demo with exit(0); the console comes back.
#
#   scripts/gui-e2e.sh files    # Files v1 (stage 8)
#
# `files` mode runs `compositor term`, moves term below where Files will
# be, makes /tmp/f (a PNG and two text files) and /tmp/g (two PNGs) and
# starts `files` from term (its log on the console):
#   F1. The list shows the folder's entries by name (the tree), the first
#       row's text preview reaches the inspector (its lines in the tree).
#   F2. Down: the PNG's preview (an Image node, its pixels on screen);
#       the list did not move (the layout is the folder's, P2.3).
#   F3. Backspace goes up with the folder selected, Enter goes back in;
#       the path bar takes /tmp/g, a folder of pictures: layout bottom
#       (the inspector under the list); Space toggles the full preview.
#   F4. Enter on a PNG opens it with imgview (/mnt/etc/gui/open).
#   F5. A provider killed mid-preview (FILES_PREVIEW_TEST=sleep) is
#       reported (SIGKILL) and the app still answers keys; a provider
#       that hangs is stopped at the timeout and said so.
#   F6. A provider that tries to open another file is refused ECAPMODE
#       (errno 135) and /proc/capdenials says so; its preview still works.
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
cursor() { # cursor <png> -> "x y" of the cursor tip: where the top of gui::compositor::CURSOR's bitmap is, pixel for pixel (black text on a
           # white menu would fool a looser test)
    python3 -c "
from PIL import Image
im = Image.open('$1').convert('RGB'); W, H = im.size; px = im.load()
rows = ['X', 'XX', 'X.X', 'X..X', 'X...X', 'X....X', 'X.....X', 'X......X', 'X.......X', 'X........X']
want = [(dx, dy, (0, 0, 0) if c == 'X' else (255, 255, 255)) for dy, r in enumerate(rows) for dx, c in enumerate(r)]
for y in range(H - len(rows)):
    for x in range(W - 10):
        if px[x, y] == (0, 0, 0) and all(px[x + dx, y + dy] == c for dx, dy, c in want):
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

# Pointer and semantic-tree helpers (modes ui, files). The pointer starts at the centre and moves 1:1.
PX=640; PY=400
mv() { $Q mouse-move $(($1 - PX)) $(($2 - PY)) >/dev/null; PX=$1; PY=$2; sleep 0.3; }
clk() { mv "$1" "$2"; $Q mouse-button 1 >/dev/null; $Q mouse-button 0 >/dev/null; sleep 0.5; }
TERMBAR=; APPBAR=; APPWIN=          # "x y" of term's and the app's title bars, the app window's title
n=0
in_term() { # in_term <command>: clicks term's title bar and types the command there
    # shellcheck disable=SC2086
    clk $TERMBAR; $Q send "$1" && $Q enter
}
tree() { # tree: runs gui-tree in term, prints its lines (each prefixed "T<n> " on the serial log), gives the app the keyboard back
    n=$((n + 1))
    # to a file first: the compositor's "client gone" when gui-tree exits would interleave with lines piped to the console
    in_term "gui-tree > /tmp/t$n; sed 's/^/T$n /' /tmp/t$n > /dev/console; echo T$n-DONE > /dev/console"
    $Q wait-for "T$n-DONE" 45 >/dev/null || echo "  (gui-tree $n did not finish)" >&2
    sleep 1                         # the log file may lag the match
    grep -a "T$n " "$STATE/serial.log" | sed "s/.*T$n //" > "$OUT/tree.last"
    cat "$OUT/tree.last"
    # the app's title bar, where gui-tree says its newest window is (windows cascade, so it moves from launch to launch)
    local at; at=$(grep "^window [0-9]* \"$APPWIN\" at " "$OUT/tree.last" | tail -1 | grep -o ' at [0-9]*,[0-9]*' | tr -d ' at' | tr ',' ' ')
    if [ -n "$at" ]; then set -- $at; clk $(($1 + 150)) $(($2 - 10)); else clk $APPBAR; fi
}
app_focus() { tree >/dev/null; }    # gives the app the keyboard wherever its window is
node() { grep -m1 -- "$1"; }        # node <pattern> < tree
scr() { # scr <tree file> <node pattern> -> "cx cy right-20" of that node on the screen ($APPWIN's content origin + its @x,y wxh)
    python3 - "$1" "$2" "$APPWIN" <<'PY'
import re, sys
lines = open(sys.argv[1]).read().splitlines()
win = next(l for l in lines if l.startswith('window ') and '"%s"' % sys.argv[3] in l)
wx, wy = map(int, re.search(r' at (-?\d+),(-?\d+) ', win).groups())
l = next(l for l in lines if re.search(sys.argv[2], l))
x, y, w, h = map(int, re.search(r'@(-?\d+),(-?\d+) (\d+)x(\d+)', l).groups())
print(wx + x + w // 2, wy + y + h // 2, wx + x + w - 20)
PY
}
box() { # box <tree file> <node pattern> -> "x y w h" as the tree has it
    grep -m1 -- "$2" "$1" | grep -o '@[-0-9]*,[-0-9]* [0-9]*x[0-9]*' | tr '@,x' '   '
}

# What the look (Luna, the default) paints where nothing covers it, from the same code the compositor runs (gui/examples/theme_px.rs):
# the desktop at a point, a focused / unfocused title bar at a point (its window's frame at fx,fy), the taskbar's strip.
(cd gui && cargo build -q --example theme_px) || { echo "cannot build theme_px"; exit 99; }
TP() { gui/target/debug/examples/theme_px luna "$@"; }
desk() { TP desktop 1280 800 "$1" "$2"; }
fbar() { TP bar 1 "$1" "$2" 2000 "$3" "$4"; }   # fbar <fx> <fy> <x> <y>
ubar() { TP bar 0 "$1" "$2" 2000 "$3" "$4"; }
strip() { TP strip 1280 800 32 "$1" "$2"; }

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
    [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && ok "T1 focused term window at (40,40)" || bad "T1 title bar: $(px "$s" 45 45)"

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
    [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && ok "T3 compositor survived ^C" || bad "T3 window gone: $(px "$s" 45 45)"
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
    [ "$(px "$s" 45 45)" = "$(desk 45 45)" ] && ok "T5 window gone" || bad "T5 window still there: $(px "$s" 45 45)"

    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "T6 console and keyboard back" || bad "T6 typing does not reach ash"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e term: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

if [ "$MODE" = wm ]; then
    BLACK=0,0,0
    goto() { # goto x y: moves the pointer there, correcting from screendumps
        local tx=$1 ty=$2 i c dx dy
        for i in $(seq 40); do   # an animated window (fire) slows the CPU compositor under TCG: screendumps lag the moves
            c=$(cursor "$(shot goto)")
            # not found: it is past the right or bottom edge (its bitmap needs 10 x 10 pixels on the screen): bring it back up-left
            if [ "$c" = none ]; then $Q mouse-move -40 -40 >/dev/null; sleep 0.3; continue; fi
            set -- $c
            [ "$1" = "$tx" ] && [ "$2" = "$ty" ] && return 0
            dx=$((tx - $1)); dy=$((ty - $2))
            [ $dx -gt 120 ] && dx=120; [ $dx -lt -120 ] && dx=-120
            [ $dy -gt 120 ] && dy=120; [ $dy -lt -120 ] && dy=-120
            $Q mouse-move $dx $dy >/dev/null; sleep 0.4
        done
        echo "  (goto $tx $ty failed)" >&2; return 1
    }
    press()   { $Q mouse-button 1 >/dev/null; sleep 0.3; }
    release() { $Q mouse-button 0 >/dev/null; sleep 0.5; }
    click()   { goto "$1" "$2" && press && release; }
    button()  { # button <label>: x of the centre of the panel's button
        grep -a "panel: buttons" "$STATE/serial.log" | tail -1 | python3 -c "
import re, sys
for label, x, w in re.findall(r' (.+?)@(\d+)\+(\d+)', sys.stdin.read().split('panel: buttons', 1)[1]):
    if label == '$1': print(int(x) + int(w) // 2)"
    }
    menuitem() { # menuitem <label>: "x y" of the centre of the open menu's item, on the screen
        grep -a "panel: menu .*@" "$STATE/serial.log" | tail -1 | python3 -c "
import re, sys
for label, x, y in re.findall(r' (.+?)@(\d+),(\d+)', sys.stdin.read().split('panel: menu', 1)[1]):
    if label == '$1': print(x, y)"
    }
    launch() { # launch <label>: opens the start menu (a popup) and clicks the app
        local n; n=$(grep -ac "panel: menu .*@" "$STATE/serial.log")
        click "$(button Apps)" $PY_
        for _ in $(seq 1 50); do [ "$(grep -ac "panel: menu .*@" "$STATE/serial.log")" -gt "$n" ] && break; sleep 0.2; done
        # shellcheck disable=SC2046
        click $(menuitem "$1")
    }
    PY_=784                   # the panel's middle row (800 - 32/2)
    console() { # console <cmd>: runs it in the focused term, output to serial
        $Q send "$1 > /dev/console" && $Q enter; sleep 1.5
    }

    $Q send "compositor" && $Q enter
    $Q wait-for "panel: buttons" 60 >/dev/null || bad "W1 no panel"
    sleep 1
    s=$(shot w1)
    [ "$(px "$s" 640 795)" = "$(strip 640 795)" ] && [ "$(px "$s" 640 760)" = "$(desk 640 760)" ] \
        && ok "W1 panel on the last strip" || bad "W1 panel: $(px "$s" 640 795) above: $(px "$s" 640 760)"
    launch Terminal
    grep -aq "panel: menu.*Terminal@" "$STATE/serial.log" || bad "W1 the start menu did not open"
    $Q wait-for "term: 80x25 cells" 30 >/dev/null || bad "W1 term never started"
    sleep 2
    s=$(shot w1b)
    [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && grep -a "panel: buttons" "$STATE/serial.log" | tail -1 | grep -q " term@" \
        && ok "W1 launcher started term, focused and listed" || bad "W1 term: $(px "$s" 45 45)"

    # term: content 720x500 at (40,60); the corner just outside it.
    goto 761 561; press
    $Q mouse-move -100 -50 >/dev/null; sleep 0.3; $Q mouse-move -100 -50 >/dev/null; sleep 0.3
    s=$(shot w2a)
    [ "$(has "$s" 224,224,224 555 355 565 365)" = yes ] && ok "W2 outline while dragging" || bad "W2 no outline at the new corner"
    release; sleep 1
    grep -a "term: resized to" "$STATE/serial.log" | tail -1 | grep -q "57x20 cells" \
        && ok "W2 one resize, whole cells (57x20)" || bad "W2 $(grep -a 'term: resized' "$STATE/serial.log" | tail -1)"
    [ "$(grep -ac 'term: resized to' "$STATE/serial.log")" = 1 ] || bad "W2 more than one resize"
    console "stty size"
    grep -aq "^20 57" "$STATE/serial.log" && ok "W2 stty size: 20 57" || bad "W2 stty size wrong"
    s=$(shot w2b)
    [ "$(px "$s" 700 300)" = "$(desk 700 300)" ] && [ "$(px "$s" 500 300)" = "$BLACK" ] \
        && ok "W2 window smaller on screen" || bad "W2 screen: $(px "$s" 700 300) / $(px "$s" 500 300)"

    click 523 50                                  # maximize (frame 40..553)
    sleep 1
    console "stty size"
    s=$(shot w3a)
    [ "$(px "$s" 5 5)" = "$(fbar 0 0 5 5)" ] && [ "$(px "$s" 640 795)" = "$(strip 640 795)" ] \
        && ok "W3 maximized over the work area, panel visible" || bad "W3 max: $(px "$s" 5 5) panel $(px "$s" 640 795)"
    grep -aq "^37 142" "$STATE/serial.log" && ok "W3 stty size: 37 142" || bad "W3 stty size after maximize"
    click 1250 10                                 # restore
    sleep 1.5
    s=$(shot w3b)
    [ "$(px "$s" 5 5)" = "$(desk 5 5)" ] && [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] \
        && ok "W3 restored" || bad "W3 restore: $(px "$s" 5 5) / $(px "$s" 45 45)"

    launch "CPU monitor"
    $Q wait-for "panel: buttons.* cpumon@" 30 >/dev/null || bad "W5 cpumon never mapped"
    sleep 4
    s=$(shot w5a)
    [ "$(px "$s" 45 45)" = "$(ubar 40 40 45 45)" ] && [ "$(px "$s" 100 150)" != "$BLACK" ] \
        && ok "W5 cpumon on top, term unfocused" || bad "W5 before: $(px "$s" 45 45) / $(px "$s" 100 150)"
    click "$(button term)" $PY_
    sleep 1
    s=$(shot w5b)
    [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && [ "$(px "$s" 100 150)" = "$BLACK" ] \
        && ok "W5 the panel raised and focused term" || bad "W5 after: $(px "$s" 45 45) / $(px "$s" 100 150)"

    click 702 82                                  # cpumon's close (frame 72..712)
    $Q wait-for "Killed PID [0-9]* \\(cpumon\\): exit\\(0\\)" 10 >/dev/null && ok "W4 cpumon closed" || bad "W4 cpumon did not close"
    click 543 50                                  # term's close (frame 40..553)
    $Q wait-for "Killed PID [0-9]* \\(term\\): exit\\(0\\)" 10 >/dev/null && grep -aq "term: closed from the title bar" "$STATE/serial.log" \
        && ok "W4 term closed" || bad "W4 term did not close"
    sleep 1
    s=$(shot w4)
    [ "$(px "$s" 45 45)" = "$(desk 45 45)" ] && ok "W4 both windows gone" || bad "W4 window left: $(px "$s" 45 45)"

    launch Fire
    $Q wait-for "panel: buttons.* fire@" 30 >/dev/null || bad "W6 fire never mapped"
    sleep 3
    edge() { # left end of the (red) close button on fire's title bar, row 110 (fire's frame is at (104,104): the third window placed)
        python3 -c "
from PIL import Image
im = Image.open('$1').convert('RGB')
print(next((x for x in range(150, im.size[0]) if (lambda p: p[0] > 150 and p[1] < 130 and p[2] < 110)(im.getpixel((x, 110)))), -1))"
    }
    s=$(shot w6a); c=$(edge "$s")
    # left of the close button is the bar itself, not a maximize button
    [ "$c" -gt 0 ] && [ "$(px "$s" $((c - 10)) 110)" = "$(fbar 0 104 $((c - 10)) 110)" ] && ok "W6 fire: no maximize button" \
        || bad "W6 a maximize button on fire (close at $c, $(px "$s" $((c - 10)) 110) left of it)"
    goto $((c + 18 + 3)) 150; press; $Q mouse-move 100 0 >/dev/null; sleep 0.3; release   # just past the frame's right edge
    s=$(shot w6b)
    [ "$(edge "$s")" = "$c" ] && ok "W6 fire's size did not change ($c)" || bad "W6 fire resized: $c -> $(edge "$s")"
    click $((c + 8)) 110   # fire's close button: its animation would slow everything after
    sleep 2

    menu_open() { # opens the start menu, waits for its log line; "x y" of its first item
        local n; n=$(grep -ac "panel: menu .*@" "$STATE/serial.log")
        click "$(button Apps)" $PY_
        for _ in $(seq 1 75); do [ "$(grep -ac "panel: menu .*@" "$STATE/serial.log")" -gt "$n" ] && break; sleep 0.2; done
        menuitem Terminal
    }
    closed() { grep -ac "panel: menu closed" "$STATE/serial.log"; }
    shows() { # shows <png before> <x> <y> <want: same|differs>: polls screendumps (the CPU compositor is slow under TCG) up to ~15 s
        local i s; for i in $(seq 1 30); do
            s=$(shot "w7poll")
            if [ "$4" = differs ]; then [ "$(px "$s" "$2" "$3")" != "$(px "$1" "$2" "$3")" ] && { echo "$s"; return 0; }
            else [ "$(px "$s" "$2" "$3")" = "$(px "$1" "$2" "$3")" ] && { echo "$s"; return 0; }; fi
            sleep 0.5
        done; echo "$s"; return 1
    }
    pre=$(shot w7pre); cp "$pre" "$OUT/w7pre-kept.png"; pre="$OUT/w7pre-kept.png"
    read -r mx my <<< "$(menu_open)"
    s=$(shows "$pre" "$mx" "$my" differs)
    [ "$(px "$s" "$mx" "$my")" != "$(px "$pre" "$mx" "$my")" ] && [ "$my" -lt 768 ] && ok "W7 the menu shows above the strip" || bad "W7 no menu at $mx,$my: $(px "$s" "$mx" "$my")"
    c0=$(closed); $Q key esc
    s=$(shows "$pre" "$mx" "$my" same)
    [ "$(closed)" -gt "$c0" ] && [ "$(px "$s" "$mx" "$my")" = "$(px "$pre" "$mx" "$my")" ] && ok "W7 Escape closes it" \
        || bad "W7 Escape: closed $(closed) (was $c0), $(px "$s" "$mx" "$my") at the item"
    menu_open >/dev/null
    c0=$(closed); click 1000 300
    for _ in $(seq 1 30); do [ "$(closed)" -gt "$c0" ] && break; sleep 0.5; done
    [ "$(closed)" -gt "$c0" ] && ok "W7 a click outside closes it" || bad "W7 a click outside did not close the menu"
    menu_open >/dev/null
    # shellcheck disable=SC2046
    click $(menuitem "9x moderno")
    $Q wait-for "panel: theme 9x$" 10 >/dev/null || bad "W7 the menu did not set the theme"
    sleep 2
    s=$(shot w7c)
    [ "$(px "$s" 640 795)" != "$(strip 640 795)" ] && ok "W7 9x's taskbar ($(px "$s" 640 795))" || bad "W7 the strip did not change"
    menu_open >/dev/null
    # shellcheck disable=SC2046
    click $(menuitem "Luna 2026")
    $Q wait-for "panel: theme luna$" 10 >/dev/null && ok "W7 back to Luna" || bad "W7 could not go back to Luna"

    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "console and keyboard back" || bad "typing does not reach ash"
    # The kernel logs every exit as "Killed PID n (name): ...".
    grep -a "Killed PID [0-9]* \(compositor\|panel\)" "$STATE/serial.log" | grep -vq "exit(0)" && bad "compositor or panel died"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e wm: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

if [ "$MODE" = std ]; then
    $Q send "compositor /mnt/bin/hello-window" && $Q enter
    $Q wait-for "hello-window: ready 320x200" 60 >/dev/null || bad "S1 hello-window never got ready"
    sleep 2
    s=$(shot s1)
    [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && ok "S1 focused window at (40,40)" || bad "S1 title bar: $(px "$s" 45 45)"
    # content (10,10): red 10*255/320, green 0x40, blue 10*255/200
    [ "$(px "$s" 50 70)" = "7,64,12" ] && ok "S1 its gradient at content (10,10)" || bad "S1 content (10,10): $(px "$s" 50 70)"
    $Q mouse-move -440 -250; sleep 1         # the cursor starts at the centre (640,400): now (200,150), content (160,90)
    $Q wait-for "hello-window: motion 160 90" 10 >/dev/null && ok "S2 motion at content (160,90)" \
        || bad "S2 last motion: $(grep -a 'hello-window: motion' "$STATE/serial.log" | tail -1)"
    sleep 1
    s=$(shot s2)
    [ "$(px "$s" 195 145)" = "255,255,255" ] && ok "S2 the square under the pointer" || bad "S2 at (195,145): $(px "$s" 195 145)"
    $Q mouse-button 1; sleep 0.5; $Q mouse-button 0
    $Q send "ab"; sleep 1
    log=$(grep -a "hello-window:" "$STATE/serial.log")
    grep -q "button 0x110 down" <<<"$log" && grep -q "button 0x110 up" <<<"$log" && ok "S3 left click reached it" || bad "S3 no button events"
    grep -q "key 30 down" <<<"$log" && grep -q "key 48 up" <<<"$log" && ok "S3 keys a,b reached it" || bad "S3 no key events"
    $Q key esc
    $Q wait-for "hello-window: bye" 15 >/dev/null && ok "S4 Esc ends it" || bad "S4 hello-window did not quit"
    sleep 1
    grep -aq "Killed PID [0-9]* (hello-window): exit(0)" "$STATE/serial.log" || bad "S4 exit: $(grep -a 'Killed PID [0-9]* (hello' "$STATE/serial.log" | tail -1)"
    s=$(shot s4)
    [ "$(px "$s" 45 45)" = "$(desk 45 45)" ] && ok "S4 window gone" || bad "S4 window still there: $(px "$s" 45 45)"
    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "S4 console and keyboard back" || bad "S4 typing does not reach ash"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e std: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

if [ "$MODE" = ui ]; then
    TERMBAR="700 468"; APPBAR="300 80"; APPWIN=ui-demo
    $Q send "compositor /mnt/bin/ui-demo term" && $Q enter
    $Q wait-for "ui-demo: ready" 60 >/dev/null || bad "U1 ui-demo never got ready"
    $Q wait-for "term: 80x25" 30 >/dev/null || bad "U1 term never started"
    sleep 2
    # term (40,40, 720 wide) is under ui-demo (72,72): drag it down out of the way, so clicking one never covers the other
    mv 700 48; $Q mouse-button 1 >/dev/null; mv 700 258; mv 700 468; $Q mouse-button 0 >/dev/null; sleep 1
    tree > "$OUT/t1"
    grep -q '^window [0-9]* "ui-demo" at ' "$OUT/t1" && ok "U1 gui-tree lists ui-demo's window" || bad "U1 no ui-demo window in: $(head -3 "$OUT/t1")"
    node 'Button#10 "Add"' < "$OUT/t1" | grep -q "{Click,Focus}" && ok "U1 the Add button" || bad "U1 no Add button"
    node 'TextInput#11 "Search"' < "$OUT/t1" >/dev/null && ok "U1 the Search field" || bad "U1 no Search field"
    [ "$(grep -c 'ListBoxOption.*"/' "$OUT/t1")" = 5 ] && ok "U1 five places" || bad "U1 places: $(grep -c 'ListBoxOption.*"/' "$OUT/t1")"
    rows=$(grep -c 'ListBoxOption.*/10000)' "$OUT/t1")
    node 'ListBox#12 "Items"' < "$OUT/t1" | grep -q "(of 10000)" && [ "$rows" -ge 8 ] && [ "$rows" -le 14 ] \
        && ok "U1 a 10000-row list, $rows rows in the tree" || bad "U1 list: $(node 'ListBox#12' < "$OUT/t1"), $rows rows"

    $Q key tab; $Q key tab; sleep 0.5      # Add, then Search
    $Q send "hola"; $Q enter; sleep 1
    $Q wait-for "ui-demo: action Submitted.11." 10 >/dev/null && ok "U2 Enter in the field: Submitted" || bad "U2 no Submitted"
    $Q key tab; $Q key tab; sleep 0.5      # Places, Items
    $Q key end; sleep 1
    $Q wait-for "ui-demo: action Selected { list: 12, row: 9999 }" 10 >/dev/null && ok "U3 End selects the last row" || bad "U3 End"
    tree > "$OUT/t2"
    node 'TextInput#11' < "$OUT/t2" | grep -q '= "hola"' && ok "U2 the field's value is in the tree" || bad "U2 field: $(node 'TextInput#11' < "$OUT/t2")"
    rows=$(grep -c 'ListBoxOption.*/10000)' "$OUT/t2")
    node '(10000/10000)' < "$OUT/t2" | grep -q '"date 09999".*\[selected\]' && [ "$rows" -le 14 ] \
        && ok "U3 row 10000 selected and in the tree, $rows rows" || bad "U3 last row: $(node '(10000/10000)' < "$OUT/t2")"
    $Q send "d"; sleep 1.5
    s=$(shot u3)
    tree > "$OUT/t3"
    node 'ListBoxOption' < <(grep '\[selected\]' "$OUT/t3" | grep '/10000)') | grep -q '"date 00003".*(4/10000)' \
        && ok "U3 typing d found date 00003" || bad "U3 find: $(grep '/10000).*selected' "$OUT/t3")"
    node 'Label#13' < "$OUT/t3" | grep -q '"selected date 00003"' && ok "U3 the status label says so" || bad "U3 label: $(node 'Label#13' < "$OUT/t3")"
    read -r cx cy rx <<< "$(scr "$OUT/t3" 'date 00003.*selected')"
    [ "$(px "$s" "$rx" "$cy")" = "49,106,197" ] && ok "U3 the row is Luna's selection blue on screen" || bad "U3 row pixel at $rx,$cy: $(px "$s" "$rx" "$cy")"

    read -r bx by _ <<< "$(scr "$OUT/t3" 'Button#10')"
    clk "$bx" "$by"
    $Q wait-for "ui-demo: action Clicked.10." 10 >/dev/null && ok "U4 a click on the button" || bad "U4 no Clicked"
    read -r cx cy _ <<< "$(scr "$OUT/t3" 'ListBoxOption.*"cherry 00006"')"
    mv "$cx" "$cy"; $Q mouse-button 1 >/dev/null; $Q mouse-button 0 >/dev/null; $Q mouse-button 1 >/dev/null; $Q mouse-button 0 >/dev/null
    $Q wait-for "ui-demo: action Activated { list: 12, row: 6 }" 10 >/dev/null && ok "U4 a double click opens the row" || bad "U4 no Activated (row 6)"

    # the wheel over the list: three notches down, one up = 6 rows (three a notch) from the top; a wrong sign gives 3 (clamped at the top)
    first() { grep -m1 'ListBoxOption.*/10000)' "$1" | grep -o '([0-9]*/10000)' | grep -o '^([0-9]*' | tr -d '('; }
    tree > "$OUT/t4"; p0=$(first "$OUT/t4")
    read -r lx ly _ <<< "$(scr "$OUT/t4" 'ListBox#12 "Items"')"
    mv "$lx" "$ly"; for dz in -1 -1 -1 1; do $Q mouse-move 0 0 $dz >/dev/null; sleep 0.3; done; sleep 1
    tree > "$OUT/t5"; p1=$(first "$OUT/t5")
    [ -n "$p0" ] && [ "$p1" = $((p0 + 6)) ] && ok "U4 the wheel scrolls the list (first row $p0 -> $p1)" || bad "U4 wheel: first row $p0 -> $p1"

    $Q key esc
    $Q wait-for "ui-demo: bye" 15 >/dev/null && ok "U5 Esc ends it" || bad "U5 ui-demo did not quit"
    sleep 1
    grep -aq "Killed PID [0-9]* (ui-demo): exit(0)" "$STATE/serial.log" || bad "U5 exit: $(grep -a 'Killed PID [0-9]* (ui-demo' "$STATE/serial.log" | tail -1)"
    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "U5 console and keyboard back" || bad "U5 typing does not reach ash"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e ui: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

if [ "$MODE" = files ]; then
    TERMBAR="400 708"; APPBAR="300 80"; APPWIN=Files
    log() { grep -a "files: " "$STATE/serial.log"; }
    ready_pid() { log | grep -a "files: ready" | tail -1 | grep -o "pid [0-9]*" | grep -o "[0-9]*"; }
    # what happened after an action: `mark` before it (k and in_term do), `waitm` polls only the lines since
    M=1
    mark() { M=$(($(wc -l < "$STATE/serial.log") + 1)); }
    waitm() { local i; for i in $(seq $(($2 * 2))); do tail -n +"$M" "$STATE/serial.log" | grep -aqE "$1" && return 0; sleep 0.5; done; return 1; }
    k() { mark; $Q key "$@"; }
    in_term() { mark; clk $TERMBAR; $Q send "$1" && $Q enter; }
    $Q send "compositor term" && $Q enter
    $Q wait-for "term: 80x25" 30 >/dev/null || bad "F1 term never started"
    sleep 2
    # term (40,40) down out of where the Files windows (cascading from 72,72, 780x490 each) will be: its title bar at y 708
    mv 700 48; $Q mouse-button 1 >/dev/null; mv 700 400; mv 700 700; $Q mouse-button 0 >/dev/null; sleep 1
    in_term "mkdir /tmp/f /tmp/g; cp /mnt/usr/share/icons/128x128/sample.png /tmp/f/pic.png; printf 'hola\\nmundo\\n' > /tmp/f/notes.txt; echo x > /tmp/f/zz.txt; cp /tmp/f/pic.png /tmp/g/a.png; cp /tmp/f/pic.png /tmp/g/b.png"
    sleep 1
    in_term "files /tmp/f > /dev/console 2>&1 &"
    waitm "files: preview notes.txt: text, 2 lines" 90 || bad "F1 no text preview of notes.txt: $(log | tail -3)"
    log | grep -q "files: at /tmp/f (3 items, layout right)" && ok "F1 /tmp/f: 3 items, preview on the right" || bad "F1 $(log | grep 'files: at' | tail -1)"
    fpid=$(ready_pid)
    clk $APPBAR
    tree > "$OUT/t1"
    for f in notes.txt pic.png zz.txt; do
        grep -q "ListBoxOption#[0-9]* \"$f\"" "$OUT/t1" || bad "F1 no row $f in the tree"
    done
    grep -q 'ListBoxOption#[0-9]* "notes.txt" = "11 B; Text; ' "$OUT/t1" && ok "F1 the rows, with size, type and date" || bad "F1 rows: $(grep ListBoxOption "$OUT/t1" | head -3)"
    grep -q 'Label#[0-9]* "hola"' "$OUT/t1" && grep -q 'Label#[0-9]* "mundo"' "$OUT/t1" && ok "F1 the text preview is in the inspector" || bad "F1 no text lines"
    lb1=$(box "$OUT/t1" 'ListBox#8 "Files"')

    k down
    waitm "files: preview pic.png: image 128x128" 30 && ok "F2 Down: pic.png previewed by files-preview" || bad "F2 no image preview: $(log | tail -2)"
    sleep 1
    s=$(shot f2)
    tree > "$OUT/t2"
    read -r ix iy _ <<< "$(scr "$OUT/t2" 'Image#[0-9]* "preview"')"
    [ -n "$ix" ] && [ "$(px "$s" "$ix" "$iy")" != "236,233,216" ] && ok "F2 the picture is on screen ($(px "$s" "$ix" "$iy") at $ix,$iy)" || bad "F2 picture: $(px "$s" "${ix:-0}" "${iy:-0}") at ${ix:-?},${iy:-?}"
    [ "$(box "$OUT/t2" 'ListBox#8 "Files"')" = "$lb1" ] && ok "F2 the list did not move ($lb1)" || bad "F2 list moved: $lb1 -> $(box "$OUT/t2" 'ListBox#8')"

    k backspace
    waitm "files: at /tmp ." 15 && ok "F3 Backspace: up to /tmp" || bad "F3 Backspace"
    sleep 1; k ret
    waitm "files: at /tmp/f .3 items" 15 && [ "$(log | grep -c 'files: at /tmp/f (')" -ge 2 ] \
        && ok "F3 Enter on the selected folder (f) goes back in" || bad "F3 Enter did not reopen /tmp/f"
    k tab; k tab; sleep 0.5    # Up, then the path bar
    k backspace; $Q send "g"; k ret
    waitm "files: at /tmp/g .2 items, layout bottom." 15 && ok "F3 the path bar: /tmp/g, pictures: layout bottom" || bad "F3 $(log | grep 'files: at' | tail -1)"
    waitm "files: preview a.png: image" 30 || bad "F3 no preview of a.png"
    sleep 1
    tree > "$OUT/t3"
    read -r _ ly _ lh <<< "$(box "$OUT/t3" 'ListBox#8 "Files"')"
    read -r _ py _ _ <<< "$(box "$OUT/t3" 'Pane#10 "Inspector"')"
    [ -n "$py" ] && [ "$py" -ge $((ly + lh)) ] && ok "F3 the inspector is under the list" || bad "F3 list y=$ly h=$lh, inspector y=$py"
    k spc
    waitm "files: quick look on" 10 && ok "F3 Space: the full preview" || bad "F3 Space did nothing"
    k spc
    waitm "files: quick look off" 10 || bad "F3 Space again did not close it"

    k ret
    waitm "files: opened /tmp/g/a.png with imgview" 15 && ok "F4 Enter opens a PNG with imgview" || bad "F4 $(log | tail -1)"
    waitm "imgview: /tmp/g/a.png -> 128x128" 30 && ok "F4 imgview loaded it" || bad "F4 imgview did not load it"
    ipid=$(log | grep -o "with imgview (pid [0-9]*)" | tail -1 | grep -o "[0-9]*")
    sleep 1
    in_term "kill $ipid $fpid"
    waitm "Killed PID $fpid .files" 15 || bad "F4 files (pid '$fpid') did not end on SIGTERM"

    # F5: a provider that hangs (the test hook) is killed by hand, then one is stopped by the timeout
    in_term "FILES_PREVIEW_TEST=sleep:60000 FILES_PREVIEW_TIMEOUT_MS=60000 files /tmp/f > /dev/console 2>&1 &"
    waitm "files: preview notes.txt: files-preview started .pid" 90 || bad "F5 no provider started"
    sleep 2
    ppid=$(log | grep -o "notes.txt: files-preview started (pid [0-9]*)" | tail -1 | grep -o "[0-9]*")
    fpid=$(ready_pid)
    in_term "kill -9 $ppid"
    waitm "files: preview notes.txt: failed: files-preview was killed by signal 9 .SIGKILL." 15 \
        && ok "F5 a killed provider is reported (SIGKILL)" || bad "F5 $(log | tail -2)"
    app_focus; k down
    waitm "files: preview pic.png: files-preview started" 15 && ok "F5 the app still answers keys" || bad "F5 no answer to Down"
    in_term "kill $fpid"
    waitm "Killed PID $fpid .files" 15 || bad "files (pid '$fpid') did not end on SIGTERM"
    in_term "FILES_PREVIEW_TEST=sleep:60000 FILES_PREVIEW_TIMEOUT_MS=3000 files /tmp/f > /dev/console 2>&1 &"
    waitm "files: preview notes.txt: failed: files-preview did not answer in 3000 ms; it was stopped" 90 \
        && ok "F5 a hung provider is stopped at the timeout" || bad "F5 no timeout: $(log | tail -2)"
    fpid=$(ready_pid)
    in_term "kill $fpid"
    waitm "Killed PID $fpid .files" 15 || bad "files (pid '$fpid') did not end on SIGTERM"

    # F6: the provider cannot open anything but its file
    in_term "FILES_PREVIEW_TEST=escape files /tmp/f > /dev/console 2>&1 &"
    waitm "files: preview notes.txt: text, 2 lines" 90 || bad "F6 no preview with the escape test"
    log | grep -q "files: files-preview said: files-preview: escape test: open /mnt/etc/gui/apps: .*(os error 135)" \
        && ok "F6 the provider's open of another file: ECAPMODE (135)" || bad "F6 $(log | grep 'escape test' | tail -1)"
    in_term "sed 's/^/CAPD /' /proc/capdenials | grep gui/apps > /dev/console"
    waitm "^CAPD [0-9]+ .*/mnt/etc/gui/apps" 10 && ok "F6 /proc/capdenials records it: $(tail -n +"$M" "$STATE/serial.log" | grep -a '^CAPD' | tail -1 | cut -c1-100)" \
        || bad "F6 not in /proc/capdenials"
    fpid=$(ready_pid)
    in_term "kill $fpid"
    waitm "Killed PID $fpid .files" 15 || bad "files (pid '$fpid') did not end on SIGTERM"
    $Q key ctrl-alt-backspace; sleep 2
    $Q send "echo console-is-back" && $Q enter
    $Q wait-for "^.fb. console-is-back" 10 >/dev/null && ok "console and keyboard back" || bad "typing does not reach ash"
    grep -aq "KERNEL PANIC" "$STATE/serial.log" && bad "kernel panic in the log"
    $Q stop >/dev/null 2>&1
    echo "gui-e2e files: $([ $fails = 0 ] && echo PASS || echo "FAIL ($fails)")"
    exit $fails
fi

if [ "$MODE" = text ]; then
    X0=40; Y0=60           # content origin of the first window
    TBG=30,33,39           # textdemo's background
    $Q send "compositor /mnt/bin/textdemo" && $Q enter
    $Q wait-for "textdemo: ready" 120 >/dev/null || bad "X1 textdemo never got ready"
    sleep 2
    s=$(shot x1)
    [ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && ok "X1 focused textdemo window at (40,40)" || bad "X1 title bar: $(px "$s" 45 45)"
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
    [ "$(px "$s" 45 45)" = "$(desk 45 45)" ] && ok "X3 window gone" || bad "X3 window still there: $(px "$s" 45 45)"
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
[ "$(px "$s" 45 45)" = "$(fbar 40 40 45 45)" ] && ok "1 focused title bar at (40,40)" || bad "1 title bar: $(px "$s" 45 45)"
c=$(px "$s" 100 150); [ "$c" != "$(desk 100 150)" ] && ok "1 window content at (100,150)" || bad "1 no content at (100,150)"
[ "$(cursor "$s")" = "640 400" ] && ok "1 cursor at the centre" || bad "1 cursor at $(cursor "$s")"

$Q mouse-move -440 -350; sleep 0.5
s=$(shot two)
[ "$(cursor "$s")" = "200 50" ] && ok "2 cursor moved 1:1 to (200,50)" || bad "2 cursor at $(cursor "$s")"

$Q mouse-button 1; $Q mouse-move 150 100; $Q mouse-move 150 100; $Q mouse-button 0; sleep 0.5
s=$(shot three)
[ "$(px "$s" 345 245)" = "$(fbar 340 240 345 245)" ] && ok "3 title bar dragged to (340,240)" || bad "3 title at (345,245): $(px "$s" 345 245)"
[ "$(px "$s" 100 150)" = "$(desk 100 150)" ] && ok "3 old place is background" || bad "3 old place: $(px "$s" 100 150)"

$Q mouse-move 0 100; $Q mouse-button 2; $Q mouse-button 0
$Q send "ab"; sleep 1
log=$(grep -a "gui_demo:" "$STATE/serial.log")
grep -q "button 0x111 down" <<<"$log" && ok "4 right click reached the window" || bad "4 no button event"
grep -q "key 30 down" <<<"$log" && grep -q "key 48 up" <<<"$log" && ok "4 keys a,b reached the window" || bad "4 no key events"
$Q mouse-move 0 0 1; sleep 0.3; $Q mouse-move 0 0 -1; sleep 1   # the wheel: a notch up, a notch down
log=$(grep -a "gui_demo:" "$STATE/serial.log")
grep -q "gui_demo: wheel 1$" <<<"$log" && grep -q "gui_demo: wheel -1$" <<<"$log" && ok "4 the wheel reached the window (+1 up, -1 down)" || bad "4 wheel: $(grep 'wheel' <<<"$log" | tr '\n' ' ')"

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
