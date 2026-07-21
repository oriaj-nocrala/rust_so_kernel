// doom-port/doomgeneric_sound_constanos.c
//
// sound_module_t DG_sound_module — sound effects backend for this kernel's
// AC97 driver (kernel/src/ac97.rs, exposed as /dev/dsp). See
// doomgeneric_constanos.c's header comment for the other platform
// primitives (fb/input/wad); this file is the audio counterpart, kept
// separate since it's a self-contained ~200 line subsystem with its own
// concerns (DMX decode, resampling, mixing).
//
// Scope: sound effects only, no music. This doomgeneric fork ships no
// music/MIDI synthesis backend at all (no i_oplmusic.c, no software OPL
// emulator in the submodule) — enabling music would mean vendoring a whole
// FM synth from elsewhere, a separate undertaking from wiring up the audio
// *driver*, which is what this file does. DG_music_module is intentionally
// not defined; i_sound.c's sound_modules[] list only requires
// DG_sound_module.
//
// /dev/dsp is a fixed-format PCM sink: 48000 Hz, stereo, signed 16-bit
// little-endian, no negotiation (matches AC97's native non-VRA operating
// point exactly — see ac97.rs's module doc). DOOM's own sound effects are
// DMX-format lumps: 8-bit unsigned mono PCM at whatever rate the WAD
// baked in (usually 11025 Hz), wrapped in an 8-byte header plus 16 bytes
// of padding at the front and back that DMX itself skips. This module
// decodes that lazily (once per sfx, first time it's played — same split
// i_sdlsound.c's LockSound/CacheSFX uses) and resamples with a simple
// 16.16 fixed-point nearest-neighbor step during mixing, since exact
// interpolation quality doesn't matter for short sound effects.

#include "doomkeys.h"
#include "doomgeneric.h"
#include "i_sound.h"
#include "w_wad.h"
#include "z_zone.h"

#include <fcntl.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define OUTPUT_RATE 48000
#define MIX_FRAMES 2048 // ~42ms per Update() call — Update() fires ~35x/sec (~28ms apart), leaving jitter headroom
#define MAX_CHANNELS 16 // matches i_sdlsound.c's own NUM_CHANNELS

static int s_dspFd = -1;

// Decoded-once cache, pointed to by sfxinfo_t::driver_data (void*) so a
// sound only gets DMX-parsed the first time it's ever played, same as
// i_sdlsound.c's CacheSFX/LockSound split.
typedef struct {
    unsigned char *samples; // raw 8-bit unsigned PCM, malloc'd, kept forever
    unsigned int length;    // sample count
    unsigned int samplerate;
} decoded_sfx_t;

typedef struct {
    decoded_sfx_t *sfx;
    unsigned int pos_fixed;  // 16.16 fixed-point index into sfx->samples
    unsigned int step_fixed; // 16.16 fixed-point: sfx->samplerate / OUTPUT_RATE
    int left_gain;           // 0..127-ish, see UpdateSoundParams
    int right_gain;
    int active;
} channel_t;

static channel_t s_channels[MAX_CHANNELS];

// ── DMX decode ───────────────────────────────────────────────────────────

static void GetSfxLumpName(sfxinfo_t *sfx, char *buf, size_t buf_len)
{
    if (sfx->link != NULL) {
        sfx = sfx->link;
    }
    // Real snprintf isn't pulled in here to keep this file's includes
    // minimal — plain concatenation is enough for the fixed "ds" prefix.
    buf[0] = 'd';
    buf[1] = 's';
    size_t i = 0;
    while (sfx->name[i] != '\0' && i < buf_len - 3) {
        buf[2 + i] = sfx->name[i];
        i++;
    }
    buf[2 + i] = '\0';
}

static decoded_sfx_t *DecodeSfx(sfxinfo_t *sfxinfo)
{
    int lumpnum = sfxinfo->lumpnum;
    unsigned char *data = (unsigned char *)W_CacheLumpNum(lumpnum, PU_STATIC);
    unsigned int lumplen = (unsigned int)W_LumpLength((unsigned int)lumpnum);

    if (lumplen < 8 || data[0] != 0x03 || data[1] != 0x00) {
        return NULL; // not a valid DMX digitized-sound lump
    }

    unsigned int samplerate = data[2] | (data[3] << 8);
    unsigned int length = data[4] | (data[5] << 8) | (data[6] << 16) | ((unsigned int)data[7] << 24);

    if (length > lumplen - 8 || length <= 48) {
        return NULL; // DMX itself won't play sounds this short/malformed
    }

    // DMX pads 16 bytes at the front and 16 at the back of the sample
    // region; the real samples start 8 bytes into what's left after
    // skipping the front pad (i.e. offset 24 from the very start of the
    // lump), and length has already had both pads subtracted.
    unsigned int real_length = length - 32;
    unsigned char *real_data = data + 24;

    decoded_sfx_t *out = malloc(sizeof(decoded_sfx_t));
    out->samples = malloc(real_length);
    memcpy(out->samples, real_data, real_length);
    out->length = real_length;
    out->samplerate = samplerate;

    W_ReleaseLumpNum(lumpnum); // done with the raw WAD lump, our own copy is permanent

    return out;
}

// ── sound_module_t callbacks ─────────────────────────────────────────────

static boolean DG_SND_Init(boolean use_sfx_prefix)
{
    (void)use_sfx_prefix; // always true in practice for Doom/Freedoom WADs
    s_dspFd = open("/dev/dsp", O_WRONLY);
    memset(s_channels, 0, sizeof(s_channels));
    return s_dspFd >= 0;
}

static void DG_SND_Shutdown(void)
{
    if (s_dspFd >= 0) {
        close(s_dspFd);
        s_dspFd = -1;
    }
}

static int DG_SND_GetSfxLumpNum(sfxinfo_t *sfxinfo)
{
    char namebuf[16];
    GetSfxLumpName(sfxinfo, namebuf, sizeof(namebuf));
    return W_GetNumForName(namebuf);
}

// Simple linear pan law: vol 0..127, sep 0..254 (0=left, 127=center,
// 254=right) — the conventional Doom source-port ranges (see s_sound.c's
// CheckVolumeSeparation). Exact fidelity doesn't matter for sfx.
static void ComputeGains(int vol, int sep, int *left, int *right)
{
    *left = (vol * (254 - sep)) / 254;
    *right = (vol * sep) / 254;
}

static void DG_SND_UpdateSoundParams(int channel, int vol, int sep)
{
    if (channel < 0 || channel >= MAX_CHANNELS) return;
    ComputeGains(vol, sep, &s_channels[channel].left_gain, &s_channels[channel].right_gain);
}

static int DG_SND_StartSound(sfxinfo_t *sfxinfo, int channel, int vol, int sep)
{
    if (channel < 0 || channel >= MAX_CHANNELS) return -1;

    if (sfxinfo->driver_data == NULL) {
        sfxinfo->driver_data = DecodeSfx(sfxinfo);
        if (sfxinfo->driver_data == NULL) {
            return -1; // invalid/malformed lump — nothing to play
        }
    }

    channel_t *c = &s_channels[channel];
    c->sfx = (decoded_sfx_t *)sfxinfo->driver_data;
    c->pos_fixed = 0;
    c->step_fixed = (unsigned int)(((uint64_t)c->sfx->samplerate << 16) / OUTPUT_RATE);
    ComputeGains(vol, sep, &c->left_gain, &c->right_gain);
    c->active = 1;

    return channel;
}

static void DG_SND_StopSound(int channel)
{
    if (channel < 0 || channel >= MAX_CHANNELS) return;
    s_channels[channel].active = 0;
}

static boolean DG_SND_SoundIsPlaying(int channel)
{
    if (channel < 0 || channel >= MAX_CHANNELS) return false;
    return s_channels[channel].active != 0;
}

static void DG_SND_CacheSounds(sfxinfo_t *sounds, int num_sounds)
{
    (void)sounds; (void)num_sounds; // lazy-decode on first StartSound instead
}

static int16_t ClipToS16(int32_t v)
{
    if (v > 32767) return 32767;
    if (v < -32768) return -32768;
    return (int16_t)v;
}

static void DG_SND_Update(void)
{
    if (s_dspFd < 0) return;

    static int16_t mixbuf[MIX_FRAMES * 2]; // interleaved L/R

    for (int frame = 0; frame < MIX_FRAMES; frame++) {
        int32_t mixL = 0, mixR = 0;

        for (int ch = 0; ch < MAX_CHANNELS; ch++) {
            channel_t *c = &s_channels[ch];
            if (!c->active) continue;

            unsigned int idx = c->pos_fixed >> 16;
            if (idx >= c->sfx->length) {
                c->active = 0;
                continue;
            }

            int32_t sample = ((int32_t)c->sfx->samples[idx] - 128) * 256; // 8-bit unsigned -> 16-bit signed
            mixL += (sample * c->left_gain) / 127;
            mixR += (sample * c->right_gain) / 127;

            c->pos_fixed += c->step_fixed;
        }

        mixbuf[frame * 2 + 0] = ClipToS16(mixL);
        mixbuf[frame * 2 + 1] = ClipToS16(mixR);
    }

    // write() may return less than the full buffer (blocking-until-space
    // contract, see dev_dsp.rs) — loop until it's all sent.
    const char *p = (const char *)mixbuf;
    size_t remaining = sizeof(mixbuf);
    while (remaining > 0) {
        long n = write(s_dspFd, p, remaining);
        if (n <= 0) break; // /dev/dsp never initialized — drop the rest of this frame
        p += n;
        remaining -= (size_t)n;
    }
}

// i_sound.c's InitSound only selects a module whose sound_devices list
// contains the current snd_sfxdevice setting (SndDeviceInList) — default
// is SNDDEVICE_SB (see m_config.c's snd_sfxdevice binding), so this list
// must include it or DG_sound_module would never be picked despite being
// the only module registered.
static snddevice_t s_sound_devices[] = { SNDDEVICE_SB };

// ── Music: no-op stub ─────────────────────────────────────────────────────
//
// i_sound.c unconditionally does `music_module = &DG_music_module;` (no
// FEATURE_SOUND guard on that specific assignment) and every I_*Music()
// wrapper calls straight through to it without checking Init()'s return
// value — so DG_music_module has to exist and every pointer has to be a
// safe no-op, even though this port has no music backend at all (see this
// file's header comment on why: no MIDI/OPL synth in this doomgeneric
// fork). `use_libsamplerate`/`libsamplerate_scale` are two more globals
// i_sound.c's I_BindSoundVariables() expects some platform file to define
// (normally the SDL backend) — real values, just unused since nothing in
// this port does sample-rate-converting playback.
int use_libsamplerate = 0;
float libsamplerate_scale = 1.0f;

static boolean DG_MUS_Init(void) { return false; }
static void DG_MUS_Shutdown(void) {}
static void DG_MUS_SetMusicVolume(int volume) { (void)volume; }
static void DG_MUS_PauseMusic(void) {}
static void DG_MUS_ResumeMusic(void) {}
static void *DG_MUS_RegisterSong(void *data, int len) { (void)data; (void)len; return NULL; }
static void DG_MUS_UnRegisterSong(void *handle) { (void)handle; }
static void DG_MUS_PlaySong(void *handle, boolean looping) { (void)handle; (void)looping; }
static void DG_MUS_StopSong(void) {}
static boolean DG_MUS_MusicIsPlaying(void) { return false; }
static void DG_MUS_Poll(void) {}

music_module_t DG_music_module =
{
    NULL, 0,
    DG_MUS_Init,
    DG_MUS_Shutdown,
    DG_MUS_SetMusicVolume,
    DG_MUS_PauseMusic,
    DG_MUS_ResumeMusic,
    DG_MUS_RegisterSong,
    DG_MUS_UnRegisterSong,
    DG_MUS_PlaySong,
    DG_MUS_StopSong,
    DG_MUS_MusicIsPlaying,
    DG_MUS_Poll,
};

sound_module_t DG_sound_module =
{
    s_sound_devices, 1,
    DG_SND_Init,
    DG_SND_Shutdown,
    DG_SND_GetSfxLumpNum,
    DG_SND_Update,
    DG_SND_UpdateSoundParams,
    DG_SND_StartSound,
    DG_SND_StopSound,
    DG_SND_SoundIsPlaying,
    DG_SND_CacheSounds,
};
