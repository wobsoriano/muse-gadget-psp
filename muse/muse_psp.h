/* PSP only additions to the Muse gadget client. */
#ifndef MUSE_PSP_H
#define MUSE_PSP_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Mix unpredictable bytes (microphone samples, button press timings) into
 * the pool behind muse_port_random. Any thread, any time, any amount.
 *
 * The PSP has no hardware random source a user mode program can reach.
 * Without bytes fed here the pool holds only the clock, a per-console
 * constant and scheduling jitter. Somebody on the same network cannot read
 * those directly, but they are far below a hardware generator. With a few
 * kilobytes of microphone noise the pool is adequate for key material. */
void muse_psp_add_entropy(const void *data, size_t len);

/* Bytes fed through muse_psp_add_entropy so far, so the app can hold back
 * muse_run until it has fed enough (4 KB of microphone audio is a fair bar). */
size_t muse_psp_entropy_added(void);

#ifdef __cplusplus
}
#endif

#endif
