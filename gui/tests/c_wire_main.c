// Host-side driver for gui/tests/c_wire.rs: exercises
// userspace/c/include/constanos_gui_wire.h so the Rust side can check it
// against gui::wire / gui::protocol.
//
//   c_wire enc          every request the header can encode, as hex on
//                       line 1 and the fds on line 2
//   c_wire dec <hex>    every event in the stream, one per line:
//                       "object opcode size arg0 arg1 ...", then "END",
//                       "PARTIAL" or "BAD" for what is left
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "constanos_gui_wire.h"

static int enc(void) {
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_create_pool(&o, 2, 17, 4096);
    guiw_create_surface(&o, 4);
    guiw_create_buffer(&o, 2, 3, 0, 320, -200, 1280, GUIW_FORMAT_XRGB8888);
    guiw_attach(&o, 4, 3);
    guiw_damage(&o, 4, -1, 2, 3, 4);
    guiw_frame(&o, 4, 16);
    guiw_commit(&o, 4);
    guiw_set_title(&o, 4, "doom");
    guiw_set_title(&o, 4, "abc");
    guiw_set_title(&o, 4, "");
    guiw_lock_pointer(&o, 4, 1);
    guiw_lock_pointer(&o, 4, 0);
    if (o.overflow) return 2;
    for (size_t i = 0; i < o.len; i++) printf("%02x", o.bytes[i]);
    printf("\n");
    for (int i = 0; i < o.nfds; i++) printf("%d ", o.fds[i]);
    printf("\n");
    return 0;
}

static int dec(const char *hex) {
    size_t n = strlen(hex) / 2;
    unsigned char *b = malloc(n + 1);
    for (size_t i = 0; i < n; i++) sscanf(hex + 2 * i, "%2hhx", &b[i]);
    size_t off = 0;
    for (;;) {
        struct guiw_msg m;
        int r = guiw_next(b + off, n - off, &m);
        if (r < 0) { printf("BAD\n"); return 0; }
        if (r == 0) { printf(off == n ? "END\n" : "PARTIAL\n"); return 0; }
        printf("%u %u %u", m.object, m.opcode, m.size);
        for (int i = 0; i < guiw_nargs(&m); i++) printf(" %d", (int)guiw_arg(&m, i));
        printf("\n");
        off += (size_t)r;
    }
}

int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "enc")) return enc();
    if (argc >= 3 && !strcmp(argv[1], "dec")) return dec(argv[2]);
    return 2;
}
