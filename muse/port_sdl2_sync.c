#ifdef PSP

#include "muse_port.h"

#include <SDL2/SDL.h>

muse_mutex *muse_port_mutex_new(void)
{
    return (muse_mutex *)SDL_CreateMutex();
}

void muse_port_mutex_free(muse_mutex *mutex)
{
    SDL_DestroyMutex((SDL_mutex *)mutex);
}

void muse_port_mutex_lock(muse_mutex *mutex)
{
    SDL_LockMutex((SDL_mutex *)mutex);
}

void muse_port_mutex_unlock(muse_mutex *mutex)
{
    SDL_UnlockMutex((SDL_mutex *)mutex);
}

uint32_t muse_port_now_ms(void)
{
    return SDL_GetTicks();
}

void muse_port_sleep_ms(uint32_t ms)
{
    SDL_Delay(ms);
}

#else

/* ISO C forbids an empty translation unit, and the library builds with
 * -Wpedantic -Werror. */
typedef int muse_port_sdl2_sync_unused;

#endif
