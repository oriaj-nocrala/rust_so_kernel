// doom-port/stub-include/SDL_mixer.h
//
// Empty stub. doomgeneric/doomgeneric/i_sound.c unconditionally
// `#include <SDL_mixer.h>` whenever FEATURE_SOUND is defined (upstream
// assumes any FEATURE_SOUND build links against SDL_mixer), but never
// actually references a single Mix_* symbol anywhere in that file — the
// include is vestigial for platforms, like this one, that provide their
// own sound_module_t (doom-port/doomgeneric_sound_constanos.c) instead of
// going through SDL. Satisfying the #include with an empty header avoids
// patching the doomgeneric submodule itself (this kernel's convention —
// see the mlibc submodule's setup-mlibc.sh patch-not-edit pattern for the
// same reasoning) while still building with -DFEATURE_SOUND.
#ifndef SDL_MIXER_H_STUB
#define SDL_MIXER_H_STUB
#endif
