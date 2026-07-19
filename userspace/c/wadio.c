// userspace/c/wadio.c
//
// Step-by-step repro of DOOM's exact WAD-open sequence against
// /dev/freedoom1.wad, at both the raw-syscall and stdio levels —
// written to isolate why W_AddFile's header read comes back wrong
// (w_file_stdc.c: fopen + M_FileLength's ftell/fseek(END)/fseek(back),
// then fseek(0)+fread(12)). Prints what every step returned so the
// failing layer (kernel lseek/read vs mlibc buffered stdio) is
// unambiguous from the serial log.

#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>

#define WAD "/dev/freedoom1.wad"

static void hex4(const char *tag, const char *buf, long n)
{
    printf("%s -> %ld [%02x %02x %02x %02x] \"%.4s\"\n",
           tag, n,
           (unsigned char)buf[0], (unsigned char)buf[1],
           (unsigned char)buf[2], (unsigned char)buf[3], buf);
}

int main(void)
{
    char buf[16];

    // ── Raw syscall level ────────────────────────────────────────────
    int fd = open(WAD, O_RDONLY);
    printf("open -> fd=%d\n", fd);
    if (fd < 0) return 1;

    memset(buf, 0, sizeof buf);
    hex4("read(12)", buf, (long)read(fd, buf, 12));

    errno = 0;
    printf("lseek(0,END) -> %ld errno=%d\n", (long)lseek(fd, 0, SEEK_END), errno);
    errno = 0;
    printf("lseek(0,SET) -> %ld errno=%d\n", (long)lseek(fd, 0, SEEK_SET), errno);

    memset(buf, 0, sizeof buf);
    hex4("read(12) after seek", buf, (long)read(fd, buf, 12));
    close(fd);

    // ── stdio level: DOOM's exact call order ─────────────────────────
    FILE *fp = fopen(WAD, "rb");
    printf("fopen -> %s\n", fp ? "ok" : "NULL");
    if (!fp) return 1;

    long saved = ftell(fp);                       // M_FileLength
    printf("ftell -> %ld\n", saved);
    printf("fseek(0,END) -> %d\n", fseek(fp, 0, SEEK_END));
    printf("ftell(at END) -> %ld\n", ftell(fp));
    printf("fseek(back,SET) -> %d\n", fseek(fp, saved, SEEK_SET));

    printf("fseek(0,SET) -> %d\n", fseek(fp, 0, SEEK_SET)); // W_StdC_Read
    memset(buf, 0, sizeof buf);
    hex4("fread(12)", buf, (long)fread(buf, 1, 12, fp));

    fclose(fp);
    return 0;
}
