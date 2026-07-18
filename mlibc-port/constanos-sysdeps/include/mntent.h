#ifndef _MNTENT_H
#define _MNTENT_H

// Declarations-only (same approach as the ironclad/vinix sysdeps ports —
// see mlibc/sysdeps/ironclad/include/mntent.h, this is a near-identical
// copy). This kernel has no /etc/mtab or mount-table concept, so none of
// these functions are actually implemented anywhere; anything that only
// needs the header to compile (BusyBox's libbb.h includes it
// unconditionally) is fine, and anything that actually *calls* one of
// these will fail at link time with an undefined symbol — a louder,
// more honest failure than silently pretending mounts work.

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
