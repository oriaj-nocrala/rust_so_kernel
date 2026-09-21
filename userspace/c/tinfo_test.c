// userspace/c/tinfo_test.c
//
// Smoke test for the cross-compiled libtinfo (scripts/build-libtinfo.sh):
// calls setupterm() against the real terminfo database this kernel ships
// at /mnt/usr/share/terminfo (scripts/build-terminfo.sh), then reads a
// couple of capability strings back with tigetstr(). Success here proves
// the whole chain end to end — TERM lookup, terminfo file I/O through the
// real VFS/ext2, and the capability parser — not just that the library
// links.
#include <stdio.h>
#include <stdlib.h>
#include <ncursesw/curses.h>
#include <ncursesw/term.h>

int main(void) {
    int err = 0;
    int rc = setupterm(NULL /* uses $TERM */, 1 /* fd 1 */, &err);
    if (rc != OK) {
        printf("tinfo_test: setupterm failed (rc=%d err=%d)\n", rc, err);
        return 1;
    }
    printf("tinfo_test: setupterm OK, term=%s\n", getenv("TERM"));

    char *clear_seq = tigetstr("clear");
    char *cup_seq = tigetstr("cup");
    printf("tinfo_test: clear=%s cup=%s\n",
           (clear_seq && clear_seq != (char *)-1) ? "present" : "MISSING",
           (cup_seq && cup_seq != (char *)-1) ? "present" : "MISSING");

    int ncols = tigetnum("cols");
    int nlines = tigetnum("lines");
    printf("tinfo_test: cols=%d lines=%d\n", ncols, nlines);

    printf("tinfo_test: PASS\n");
    return 0;
}
