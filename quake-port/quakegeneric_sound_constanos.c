// quake-port/quakegeneric_sound_constanos.c
//
// Real sound effects for Quake via /dev/dsp (this kernel's AC97 driver,
// kernel/src/ac97.rs — same device DOOM's sound port already uses, see
// doom-port/doomgeneric_sound_constanos.c). Replaces quakegeneric's own
// snd_null.c (a complete no-op stub of the S_* API sound.h declares).
//
// Unlike DOOM, there's no lower-level SNDDMA_*-style driver interface to
// hook into here: quakegeneric's vendored source tree doesn't include
// Quake's real mixing engine (snd_dma.c/snd_mix.c/snd_mem.c) at all, only
// the null stub — trimmed out entirely, not just unused. So this file
// reimplements the handful of S_* entry points a real port needs directly
// (channel bookkeeping, WAV decode, mixing), rather than plugging into an
// engine-provided mixer the way the DOOM port's sound_module_t does.
//
// Sound data: Quake's shareware sound/*.wav lumps, read straight out of
// the ext2-served pak0.pak via COM_LoadTempFile (real VFS path, same as
// everything else this port reads — see quakegeneric_constanos.c). Format
// verified directly against the shareware pak: 8-bit unsigned PCM, mono,
// 11025 Hz, standard RIFF/WAVE headers — decoded once per sfx name (first
// S_PrecacheSound call) and kept forever in a Z_Malloc'd buffer referenced
// through sfx_t's own cache_user_t.data field. That deliberately bypasses
// the real Cache_Alloc/Cache_Check LRU system entirely (no eviction is
// implemented) — the shareware episode's ~190 sound files are a modest
// total size that comfortably fits in memory all at once, so there's
// nothing to evict.
//
// Scope, matching the same simplification level already used for DOOM's
// sound port ("sound effects work, not spatialized"): dynamic one-shot
// sounds only (weapon fire, pickups, monster/player sounds — S_StartSound/
// S_StopSound), volume-only mixing (no stereo panning, no distance
// attenuation — computing real per-frame 3D panning would mean
// reimplementing snd_dma.c's own SND_Spatialize against the listener
// origin/orientation S_Update is handed, which is a separate, bigger
// undertaking from wiring up the audio *driver*). S_StaticSound (looping
// ambient sounds tied to level geometry — torches, machinery) is
// deliberately left a no-op: those are supposed to fade with player
// distance, and playing them all at a flat volume forever would very
// likely sound *worse* than having none at all, so this doesn't attempt
// it. No music — matches Quake's own snd_null.c convention and the
// project's existing "no music" scope for DOOM.

#include "quakedef.h"

#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <stdio.h>

cvar_t bgmvolume = {"bgmvolume", "1", true};
cvar_t volume = {"volume", "0.7", true};

#define QSND_MAX_CHANNELS 32
#define MAX_SFX 512
#define OUTPUT_RATE 48000
#define MIX_FRAMES 2048 // ~42ms per S_Update() call, matching the DOOM sound port's own margin

static int s_dspFd = -1;

// Decoded-once WAV cache — pointed to by sfx_t::cache.data (a plain void*
// in this trimmed-down source tree, see zone.h's cache_user_t), our own
// convention rather than the real Cache_Alloc/Cache_Check indirection.
typedef struct {
    int rate;
    int length;    // sample count
    unsigned char data[1]; // variable-length raw 8-bit unsigned PCM
} qsnd_cache_t;

typedef struct {
    qsnd_cache_t *cache;
    unsigned int pos_fixed;  // 16.16 fixed-point index into cache->data
    unsigned int step_fixed; // 16.16 fixed-point: cache->rate / OUTPUT_RATE
    int vol;                 // 0..255
    int entnum;
    int entchannel;
    int active;
} qchannel_t;

static qchannel_t s_channels[QSND_MAX_CHANNELS];

// Flat by-name registry — quakegeneric doesn't give us the real
// known_sfx[]/S_FindName machinery (that lives in snd_dma.c, not vendored
// here), so S_PrecacheSound needs its own "have we already loaded this
// name" lookup to avoid decoding the same WAV twice.
static sfx_t s_known_sfx[MAX_SFX];
static int s_num_known_sfx = 0;

static qsnd_cache_t *LoadWav(char *name)
{
    char path[MAX_QPATH + 6];
    sprintf(path, "sound/%s", name);

    byte *buf = COM_LoadTempFile(path);
    if (!buf || com_filesize <= 0) {
        return NULL;
    }

    int filesize = com_filesize;
    if (filesize < 12 || memcmp(buf, "RIFF", 4) != 0 || memcmp(buf + 8, "WAVE", 4) != 0) {
        return NULL; // not a WAV file
    }

    int pos = 12;
    int channels = 1, samplerate = 11025, bitspersample = 8;
    unsigned char *data = NULL;
    int datalen = 0;

    while (pos + 8 <= filesize) {
        unsigned char *chunk = buf + pos;
        unsigned int csize;
        memcpy(&csize, chunk + 4, 4);

        // Sanity-check the declared chunk size BEFORE trusting it for
        // anything — a chunk type this parser doesn't recognize (or a
        // corrupted size field) must stop parsing right here rather than
        // let `pos` advance by a garbage amount. Real bug this caught:
        // an unrecognized/malformed chunk's `csize` came back as a huge
        // unsigned value that went negative once cast to `int` for the
        // `pos +=` advancement below, sending `pos` wildly negative; the
        // loop condition (`pos + 8 <= filesize`) stayed satisfied
        // regardless (a very negative number is always <= filesize), so
        // it kept "parsing" out-of-bounds memory before `buf` as if it
        // were more WAV chunks, eventually treating garbage as a "data"
        // chunk with a plausible-looking length and segfaulting deep
        // inside the later memcpy of `datalen` bytes from a wild pointer.
        if (csize > (unsigned int)filesize - (unsigned int)pos - 8) {
            break; // malformed/unrecognized — stop parsing this file
        }

        if (memcmp(chunk, "fmt ", 4) == 0 && csize >= 16) {
            unsigned short ch, bps;
            unsigned int sr;
            memcpy(&ch, chunk + 8 + 2, 2);
            memcpy(&sr, chunk + 8 + 4, 4);
            memcpy(&bps, chunk + 8 + 14, 2);
            channels = ch;
            samplerate = (int)sr;
            bitspersample = bps;
        } else if (memcmp(chunk, "data", 4) == 0) {
            data = chunk + 8;
            datalen = (int)csize; // safe: csize already bounded to fit within filesize above
        }

        if (csize == 0) break; // malformed — avoid an infinite loop
        pos += 8 + (int)csize + (int)(csize & 1);
    }

    if (!data || datalen <= 0) {
        return NULL;
    }

    // Only 8-bit mono is actually present in the shareware sound set
    // (verified directly against pak0.pak) — anything else falls back to
    // "no sound" rather than mixing it wrong.
    if (bitspersample != 8 || channels != 1) {
        return NULL;
    }

    qsnd_cache_t *cache = Z_Malloc(sizeof(qsnd_cache_t) + datalen);
    cache->rate = samplerate;
    cache->length = datalen;
    memcpy(cache->data, data, datalen);

    return cache;
}

void S_Init (void)
{
    Cvar_RegisterVariable (&volume);
    Cvar_RegisterVariable (&bgmvolume);
    memset(s_channels, 0, sizeof(s_channels));
    s_num_known_sfx = 0;
    s_dspFd = open("/dev/dsp", O_WRONLY);
}

void S_AmbientOff (void)
{
}

void S_AmbientOn (void)
{
}

void S_Shutdown (void)
{
    if (s_dspFd >= 0) {
        close(s_dspFd);
        s_dspFd = -1;
    }
}

void S_TouchSound (char *sample)
{
    (void)sample; // nothing to keep alive — we never purge
}

void S_ClearBuffer (void)
{
    memset(s_channels, 0, sizeof(s_channels));
}

// See the file header comment for why this stays a no-op.
void S_StaticSound (sfx_t *sfx, vec3_t origin, float vol, float attenuation)
{
    (void)sfx; (void)origin; (void)vol; (void)attenuation;
}

static int PickChannel(int entnum, int entchannel)
{
    // Override a channel already playing for the same entity+sub-channel,
    // matching real Quake's own SND_PickChannel convention — a monster's
    // repeated attack sound replaces its own previous instance instead of
    // stacking indefinitely.
    if (entchannel != 0) {
        for (int i = 0; i < QSND_MAX_CHANNELS; i++) {
            if (s_channels[i].active && s_channels[i].entnum == entnum && s_channels[i].entchannel == entchannel) {
                return i;
            }
        }
    }
    for (int i = 0; i < QSND_MAX_CHANNELS; i++) {
        if (!s_channels[i].active) {
            return i;
        }
    }
    return 0; // all full — steal the oldest-indexed one rather than drop the new sound
}

void S_StartSound (int entnum, int entchannel, sfx_t *sfx, vec3_t origin, float fvol, float attenuation)
{
    (void)origin; (void)attenuation; // no distance/pan model — see file header

    if (!sfx || !sfx->cache.data) {
        return;
    }

    int idx = PickChannel(entnum, entchannel);
    qchannel_t *c = &s_channels[idx];
    qsnd_cache_t *cache = (qsnd_cache_t *)sfx->cache.data;

    c->cache = cache;
    c->pos_fixed = 0;
    c->step_fixed = (unsigned int)(((unsigned long long)cache->rate << 16) / OUTPUT_RATE);
    int vol255 = (int)(fvol * 255.0f);
    if (vol255 < 0) vol255 = 0;
    if (vol255 > 255) vol255 = 255;
    c->vol = vol255;
    c->entnum = entnum;
    c->entchannel = entchannel;
    c->active = 1;
}

void S_StopSound (int entnum, int entchannel)
{
    for (int i = 0; i < QSND_MAX_CHANNELS; i++) {
        if (s_channels[i].active && s_channels[i].entnum == entnum && s_channels[i].entchannel == entchannel) {
            s_channels[i].active = 0;
        }
    }
}

sfx_t *S_PrecacheSound (char *sample)
{
    if (!sample) {
        return NULL;
    }

    for (int i = 0; i < s_num_known_sfx; i++) {
        if (strcmp(s_known_sfx[i].name, sample) == 0) {
            return &s_known_sfx[i];
        }
    }

    if (s_num_known_sfx >= MAX_SFX) {
        return NULL; // registry full — drop silently, matches snd_null.c's "no sound" fallback
    }

    sfx_t *sfx = &s_known_sfx[s_num_known_sfx++];
    strncpy(sfx->name, sample, sizeof(sfx->name) - 1);
    sfx->name[sizeof(sfx->name) - 1] = '\0';
    sfx->cache.data = LoadWav(sample); // NULL on failure — S_StartSound just skips it

    return sfx;
}

void S_ClearPrecache (void)
{
    // Real Quake resets its precache-tracking list here at the start of
    // every level load; nothing to reset in this port — s_known_sfx is a
    // permanent by-name cache for the whole session, and re-precaching an
    // already-loaded name in S_PrecacheSound above is already a cheap
    // linear-scan cache hit, not a reload.
}

void S_Update (vec3_t origin, vec3_t v_forward, vec3_t v_right, vec3_t v_up)
{
    (void)origin; (void)v_forward; (void)v_right; (void)v_up;

    if (s_dspFd < 0) {
        return;
    }

    static short mixbuf[MIX_FRAMES * 2]; // interleaved L/R, s16le

    for (int frame = 0; frame < MIX_FRAMES; frame++) {
        int mix = 0;

        for (int i = 0; i < QSND_MAX_CHANNELS; i++) {
            qchannel_t *c = &s_channels[i];
            if (!c->active) {
                continue;
            }

            unsigned int idx = c->pos_fixed >> 16;
            if ((int)idx >= c->cache->length) {
                c->active = 0;
                continue;
            }

            int sample = ((int)c->cache->data[idx] - 128) * 256; // 8-bit unsigned -> 16-bit signed
            mix += (sample * c->vol) / 255;

            c->pos_fixed += c->step_fixed;
        }

        if (mix > 32767) mix = 32767;
        if (mix < -32768) mix = -32768;

        mixbuf[frame * 2 + 0] = (short)mix;
        mixbuf[frame * 2 + 1] = (short)mix; // mono, centered — no stereo pan model
    }

    // write() may return less than the full buffer (blocking-until-space
    // contract, see dev_dsp.rs) — loop until it's all sent.
    const char *p = (const char *)mixbuf;
    int remaining = (int)sizeof(mixbuf);
    while (remaining > 0) {
        long n = write(s_dspFd, p, (size_t)remaining);
        if (n <= 0) break; // /dev/dsp never opened — drop the rest of this frame
        p += n;
        remaining -= (int)n;
    }
}

void S_StopAllSounds (qboolean clear)
{
    memset(s_channels, 0, sizeof(s_channels));
    (void)clear;
}

void S_BeginPrecaching (void)
{
}

void S_EndPrecaching (void)
{
}

void S_ExtraUpdate (void)
{
    // Real Quake calls this more often than S_Update during long
    // operations (e.g. level loading) to keep audio flowing without a
    // stutter. Just mixing again here (harmless — S_Update only advances
    // each channel's own position, calling it more often just keeps the
    // /dev/dsp ring topped up) is enough; there's no separate hardware
    // ring buffer state to service on its own like a real DMA-backed port
    // would have.
    S_Update (vec3_origin, vec3_origin, vec3_origin, vec3_origin);
}

void S_LocalSound (char *s)
{
    sfx_t *sfx = S_PrecacheSound (s);
    if (!sfx) {
        return;
    }
    S_StartSound (0, 0, sfx, vec3_origin, 1.0f, 1.0f);
}
