/* Platform hooks the library links against. Each platform provides every
 * function here exactly once. port_posix.c covers the host. On the PSP,
 * port_psp.c covers the socket, random and clock group and
 * port_sdl2_sync.c covers the mutex and monotonic time group. */
#ifndef MUSE_PORT_H
#define MUSE_PORT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Mutex and monotonic time. */
typedef struct muse_mutex muse_mutex;
muse_mutex *muse_port_mutex_new(void);
void muse_port_mutex_free(muse_mutex *mutex);
void muse_port_mutex_lock(muse_mutex *mutex);
void muse_port_mutex_unlock(muse_mutex *mutex);
/* Monotonic milliseconds. Wraps after 49 days; compare with subtraction. */
uint32_t muse_port_now_ms(void);
void muse_port_sleep_ms(uint32_t ms);

/* Wall clock, unix seconds. 0 when the platform clock is not set. */
int64_t muse_port_unix_time(void);

/* Fill buf with len random bytes suitable for key material. Returns 0, or
 * nonzero when no acceptable source exists; the library then refuses to
 * start TLS or Noise rather than run on a weak generator. */
int muse_port_random(void *buf, size_t len);

/* Blocking TCP with per-call timeouts. Return values follow one rule:
 * recv/send return bytes moved (>0), MUSE_PORT_TIMEOUT (0) when the timeout
 * passed with nothing moved, or a negative MUSE_PORT_* code. */
typedef intptr_t muse_sock;
#define MUSE_PORT_TIMEOUT 0
#define MUSE_PORT_CLOSED (-1)     /* orderly close by the peer */
#define MUSE_PORT_ERROR (-2)      /* socket error */
#define MUSE_PORT_RESOLVE (-3)    /* name resolution failed */
#define MUSE_PORT_REFUSED (-4)    /* connect failed */

/* Resolve host (name or dotted quad) and connect. Returns 0 and sets *out,
 * or a negative MUSE_PORT_* code. */
int muse_port_connect(const char *host, uint16_t port, uint32_t timeout_ms, muse_sock *out);
int muse_port_recv(muse_sock sock, void *buf, size_t len, uint32_t timeout_ms);
int muse_port_send(muse_sock sock, const void *buf, size_t len, uint32_t timeout_ms);
void muse_port_close(muse_sock sock);

#ifdef __cplusplus
}
#endif

#endif
