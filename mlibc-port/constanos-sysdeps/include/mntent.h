#ifndef _MNTENT_H
#define _MNTENT_H

// setmntent/getmntent/endmntent ARE implemented (generic.cpp) — BusyBox
// `df` (no args) needs them to enumerate mounts at all, and this kernel's
// mount table is small and fixed at compile time (see fs/mod.rs's MOUNT
// LAYOUT), so hardcoding it here is a straight port of real, if static,
// data rather than a fake. addmntent/hasmntopt/getmntent_r stay
// unimplemented — nothing in this port's applet set calls them, and a
// link-time "undefined symbol" for those is a louder, more honest failure
// than silently stubbing a function nothing has validated the behavior of.

#include <stdio.h>

#define MOUNTED "/etc/mtab"

#define MNTOPT_DEFAULTS "defaults"
#define MNTOPT_RO       "ro"
#define MNTOPT_RW       "rw"
#define MNTOPT_SUID     "suid"
#define MNTOPT_NOSUID   "nosuid"
#define MNTOPT_NOAUTO   "noauto"

#ifdef __cplusplus
extern "C" {
#endif

struct mntent {
	char *mnt_fsname;
	char *mnt_dir;
	char *mnt_type;
	char *mnt_opts;
	int mnt_freq;
	int mnt_passno;
};

FILE *setmntent(const char *, const char *);
struct mntent *getmntent(FILE *);
int addmntent(FILE *, const struct mntent *);
int endmntent(FILE *);
char *hasmntopt(const struct mntent *, const char *);
struct mntent *getmntent_r(FILE *, struct mntent *, char *, int);

#ifdef __cplusplus
}
#endif

#endif /* _MNTENT_H */
