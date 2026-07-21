// Exercises the ext2 robustness work: real symlinks (fast + slow
// representations), real chmod/fchmod persistence, and triply-indirect
// block allocation for files whose offset lands past the doubly-indirect
// capacity (12 + 256 + 256*256 blocks * 1024-byte block size).
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <errno.h>

#define TRIPLE_INDIRECT_OFFSET 67383296L

int main(void) {
    int fd = open("/mnt/realfile.txt", O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0) { printf("open realfile FAILED errno=%d\n", errno); return 1; }
    write(fd, "hello-ext2\n", 11);
    close(fd);

    unlink("/mnt/symlink1");
    if (symlink("realfile.txt", "/mnt/symlink1") != 0) {
        printf("symlink FAILED errno=%d\n", errno);
        return 1;
    }

    char buf[256];
    ssize_t n = readlink("/mnt/symlink1", buf, sizeof(buf) - 1);
    if (n < 0) { printf("readlink FAILED errno=%d\n", errno); return 1; }
    buf[n] = 0;
    if (strcmp(buf, "realfile.txt") != 0) {
        printf("readlink MISMATCH: got '%s'\n", buf);
        return 1;
    }
    printf("readlink(fast): OK (%s)\n", buf);

    struct stat lst;
    if (lstat("/mnt/symlink1", &lst) != 0) { printf("lstat FAILED errno=%d\n", errno); return 1; }
    if (!S_ISLNK(lst.st_mode)) { printf("lstat type FAILED: mode=0x%x\n", lst.st_mode); return 1; }
    printf("lstat: OK (S_ISLNK, mode=0%o)\n", lst.st_mode & 0777);

    int sfd = open("/mnt/symlink1", O_RDONLY);
    if (sfd < 0) { printf("open(symlink1) dereferenced FAILED errno=%d\n", errno); return 1; }
    char content[32] = {0};
    read(sfd, content, sizeof(content) - 1);
    close(sfd);
    if (strcmp(content, "hello-ext2\n") != 0) {
        printf("dereferenced read MISMATCH: got '%s'\n", content);
        return 1;
    }
    printf("dereferenced open/read: OK\n");

    unlink("/mnt/symlink2");
    const char *longtarget = "a_very_long_symlink_target_that_does_not_fit_in_the_inode_fast_path_area_for_sure";
    if (symlink(longtarget, "/mnt/symlink2") != 0) {
        printf("symlink(long) FAILED errno=%d\n", errno);
        return 1;
    }
    n = readlink("/mnt/symlink2", buf, sizeof(buf) - 1);
    if (n < 0) { printf("readlink(long) FAILED errno=%d\n", errno); return 1; }
    buf[n] = 0;
    if (strcmp(buf, longtarget) != 0) {
        printf("readlink(long) MISMATCH: got '%s'\n", buf);
        return 1;
    }
    printf("readlink(slow): OK\n");

    if (chmod("/mnt/realfile.txt", 0600) != 0) { printf("chmod FAILED errno=%d\n", errno); return 1; }
    struct stat cst;
    if (stat("/mnt/realfile.txt", &cst) != 0) { printf("stat(after chmod) FAILED errno=%d\n", errno); return 1; }
    if ((cst.st_mode & 0777) != 0600) {
        printf("chmod MISMATCH: got 0%o\n", cst.st_mode & 0777);
        return 1;
    }
    printf("chmod: OK (0%o)\n", cst.st_mode & 0777);

    int rfd = open("/mnt/realfile.txt", O_RDONLY);
    if (rfd < 0) { printf("open(after chmod) FAILED errno=%d\n", errno); return 1; }
    if (fchmod(rfd, 0640) != 0) { printf("fchmod FAILED errno=%d\n", errno); close(rfd); return 1; }
    close(rfd);
    if (stat("/mnt/realfile.txt", &cst) != 0) { printf("stat(after fchmod) FAILED errno=%d\n", errno); return 1; }
    if ((cst.st_mode & 0777) != 0640) {
        printf("fchmod MISMATCH: got 0%o\n", cst.st_mode & 0777);
        return 1;
    }
    printf("fchmod: OK (0%o)\n", cst.st_mode & 0777);

    unlink("/mnt/bigfile.bin");
    int bfd = open("/mnt/bigfile.bin", O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (bfd < 0) { printf("open(bigfile) FAILED errno=%d\n", errno); return 1; }
    if (lseek(bfd, TRIPLE_INDIRECT_OFFSET, SEEK_SET) != TRIPLE_INDIRECT_OFFSET) {
        printf("lseek(triple-indirect) FAILED errno=%d\n", errno);
        return 1;
    }
    const char *marker = "TPL!";
    if (write(bfd, marker, 4) != 4) {
        printf("write(triple-indirect) FAILED errno=%d\n", errno);
        return 1;
    }
    close(bfd);

    struct stat bst;
    if (stat("/mnt/bigfile.bin", &bst) != 0) { printf("stat(bigfile) FAILED errno=%d\n", errno); return 1; }
    if (bst.st_size != TRIPLE_INDIRECT_OFFSET + 4) {
        printf("bigfile size MISMATCH: got %ld\n", (long)bst.st_size);
        return 1;
    }
    printf("triple-indirect alloc: OK (size=%ld)\n", (long)bst.st_size);

    int rbfd = open("/mnt/bigfile.bin", O_RDONLY);
    if (rbfd < 0) { printf("open(bigfile, read) FAILED errno=%d\n", errno); return 1; }
    if (lseek(rbfd, TRIPLE_INDIRECT_OFFSET, SEEK_SET) != TRIPLE_INDIRECT_OFFSET) {
        printf("lseek(read-back) FAILED errno=%d\n", errno);
        close(rbfd);
        return 1;
    }
    char rb[5] = {0};
    if (read(rbfd, rb, 4) != 4 || strcmp(rb, marker) != 0) {
        printf("triple-indirect read-back MISMATCH: got '%s'\n", rb);
        close(rbfd);
        return 1;
    }
    close(rbfd);
    printf("triple-indirect read-back: OK\n");

    int hfd = open("/mnt/bigfile.bin", O_RDONLY);
    lseek(hfd, 20000, SEEK_SET);
    unsigned char hz[8];
    read(hfd, hz, 8);
    close(hfd);
    for (int i = 0; i < 8; i++) {
        if (hz[i] != 0) { printf("hole read-back MISMATCH at %d: 0x%x\n", i, hz[i]); return 1; }
    }
    printf("hole zero-fill: OK\n");

    if (unlink("/mnt/bigfile.bin") != 0) { printf("unlink(bigfile) FAILED errno=%d\n", errno); return 1; }
    // Regression check: unlink() on a *fast* symlink (target inline in
    // i_block's own bytes) used to misread those text bytes as real block
    // pointers and fail with EIO trying to "free" them — see
    // free_all_blocks's doc comment in ext2.rs. symlink1 is fast
    // ("realfile.txt", well under 60 bytes); symlink2 is slow.
    if (unlink("/mnt/symlink1") != 0) { printf("unlink(fast symlink) FAILED errno=%d\n", errno); return 1; }
    if (unlink("/mnt/symlink2") != 0) { printf("unlink(slow symlink) FAILED errno=%d\n", errno); return 1; }
    if (unlink("/mnt/realfile.txt") != 0) { printf("unlink(realfile) FAILED errno=%d\n", errno); return 1; }
    printf("unlink cleanup: OK (incl. fast-symlink free_all_blocks fix)\n");

    printf("ext2_robust_test: ALL_OK\n");
    return 0;
}
