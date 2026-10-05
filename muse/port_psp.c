/* PSP sockets, wall clock and random bytes for the Muse gadget client.
 *
 * Random bytes come from a SHA-256 pool, since user mode has no hardware
 * generator. Without muse_psp_add_entropy the pool holds roughly the clock,
 * a per-console constant and scheduling jitter. Somebody on the same network
 * cannot read those directly, but they are far below a hardware generator.
 * With microphone noise fed in by the app the pool is adequate. */
#ifdef PSP

/* getentropy is a BSD name, hidden under -std=c99 without this. */
#define _DEFAULT_SOURCE

#include "muse_port.h"
#include "muse_psp.h"

#include <errno.h>
#include <limits.h>
#include <mbedtls/platform_util.h>
#include <mbedtls/sha256.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* The PSPSDK headers repeat typedefs and use enumerators beyond int, which
 * -Wpedantic -Werror rejects, and the toolchain file adds them with -I. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wpedantic"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <pspnet_apctl.h>
#include <pspnet_inet.h>
#include <pspopenpsid.h>
#include <psprtc.h>
#include <pspintrman.h>
#include <pspthreadman.h>
#include <sys/socket.h>
#pragma GCC diagnostic pop

#include "muse_internal.h"

#define CLOCK_UNSET_BEFORE 1577836800 /* 2020-01-01 */
#define LOOKUP_WAIT_MS 1500u
#define KNOWN_HOSTS 4
#define KNOWN_HOST_MS (10u * 60u * 1000u)
#define JITTER_ROUNDS 64
#define POOL_SIZE 32

/* The real time clock counts microseconds from the year 1, in UTC. The C
 * library's time() is not used: on a real PSP it returned a moment in 1969
 * while this clock was right. */
#define RTC_TICKS_AT_UNIX_EPOCH 62135596800000000ULL

int64_t muse_port_unix_time(void)
{
    u64 tick = 0;
    if (sceRtcGetCurrentTick(&tick) != 0 || tick < RTC_TICKS_AT_UNIX_EPOCH) {
        return 0;
    }
    int64_t now = (int64_t)((tick - RTC_TICKS_AT_UNIX_EPOCH) / 1000000ULL);
    return now < CLOCK_UNSET_BEFORE ? 0 : now;
}

static struct {
    uint8_t pool[POOL_SIZE];
    uint64_t counter;
    size_t added;
    int has_console_id;
    volatile int busy;
} rng;

/* The app feeds entropy from its own thread while the library draws, and
 * there is no point at which a real mutex could be created race free.
 *
 * The flag is tested and set with interrupts off, which is atomic on the
 * PSP's single core. The compiler's own atomic swap is not: it uses the
 * ll/sc instructions, and on a real PSP a thread switch between the two let
 * a release be overwritten, leaving the pool locked with no owner. Every
 * later handshake then waited forever. */
static void rng_lock(void)
{
    for (;;) {
        unsigned int interrupts = sceKernelCpuSuspendIntr();
        int held = rng.busy;
        rng.busy = 1;
        sceKernelCpuResumeIntr(interrupts);
        if (!held) {
            return;
        }
        sceKernelDelayThread(100);
    }
}

static void rng_unlock(void)
{
    rng.busy = 0;
}

/* out = SHA256(pool || a || b). out may be the pool itself. */
static int pool_hash(const void *a, size_t a_len, const void *b, size_t b_len, uint8_t out[POOL_SIZE])
{
    mbedtls_sha256_context sha;
    mbedtls_sha256_init(&sha);
    int rc = mbedtls_sha256_starts_ret(&sha, 0);
    if (rc == 0) {
        rc = mbedtls_sha256_update_ret(&sha, rng.pool, POOL_SIZE);
    }
    if (rc == 0 && a_len) {
        rc = mbedtls_sha256_update_ret(&sha, a, a_len);
    }
    if (rc == 0 && b_len) {
        rc = mbedtls_sha256_update_ret(&sha, b, b_len);
    }
    if (rc == 0) {
        rc = mbedtls_sha256_finish_ret(&sha, out);
    }
    mbedtls_sha256_free(&sha);
    return rc;
}

void muse_psp_add_entropy(const void *data, size_t len)
{
    if (!data || !len) {
        return;
    }
    rng_lock();
    if (pool_hash(data, len, NULL, 0, rng.pool) == 0) {
        rng.added += len;
    }
    rng_unlock();
}

size_t muse_psp_entropy_added(void)
{
    rng_lock();
    size_t added = rng.added;
    rng_unlock();
    return added;
}

static int pool_stir(void)
{
    struct {
        PspOpenPSID console;
        u64 rtc;
        uint32_t uptime;
        uint8_t libc[32];
        struct {
            uint32_t spins;
            uint32_t tick;
        } jitter[JITTER_ROUNDS];
    } fresh;
    memset(&fresh, 0, sizeof(fresh));
    if (!rng.has_console_id) {
        sceOpenPSIDGetOpenPSID(&fresh.console);
        rng.has_console_id = 1;
    }
    sceRtcGetCurrentTick(&fresh.rtc);
    fresh.uptime = sceKernelGetSystemTimeLow();
    /* Weak on the PSP (a Mersenne Twister seeded from the clock), mixed in
     * only because it costs nothing. A failure leaves zeros, which is fine. */
    (void)getentropy(fresh.libc, sizeof(fresh.libc));
    for (int i = 0; i < JITTER_ROUNDS; i++) {
        uint32_t before = sceKernelGetSystemTimeLow();
        uint32_t tick = before;
        uint32_t spins = 0;
        while (tick == before) {
            tick = sceKernelGetSystemTimeLow();
            spins++;
        }
        fresh.jitter[i].spins = spins;
        fresh.jitter[i].tick = tick;
    }
    int rc = pool_hash(&fresh, sizeof(fresh), NULL, 0, rng.pool);
    mbedtls_platform_zeroize(&fresh, sizeof(fresh));
    return rc;
}

int muse_port_random(void *buf, size_t len)
{
    uint8_t *out = buf;
    uint8_t block[POOL_SIZE];
    rng_lock();
    int rc = pool_stir();
    while (rc == 0 && len > 0) {
        rc = pool_hash(&rng.counter, sizeof(rng.counter), "out", 3, block);
        rng.counter++;
        size_t n = len < sizeof(block) ? len : sizeof(block);
        if (rc == 0) {
            memcpy(out, block, n);
        }
        out += n;
        len -= n;
    }
    /* Moves the pool on so a later memory dump cannot recompute this output. */
    if (pool_hash("fwd", 3, NULL, 0, rng.pool) != 0) {
        rc = -1;
    }
    rng_unlock();
    mbedtls_platform_zeroize(block, sizeof(block));
    return rc;
}

/* Sockets stay non-blocking for their whole life and every call waits in
 * sceNetInetPoll. SO_RCVTIMEO and SO_SNDTIMEO exist in sys/socket.h, but no
 * header says what value layout the PSP wants for them, and a wrong guess
 * there is a silent hang. The poll timeout is a plain int of milliseconds. */
static int wait_ready(int fd, short events, uint32_t timeout_ms)
{
    SceNetInetPollfd poll = { fd, events, 0 };
    return sceNetInetPoll(&poll, 1, timeout_ms > INT_MAX ? INT_MAX : (int)timeout_ms);
}

/* The byte count comes back in a size_t, and the C library's wrapper has
 * returned a pointer there on a real PSP. Anything that is not a count this
 * call could have moved is an error. */
static int checked_count(size_t returned, size_t len, int on_zero)
{
    int n = (int)returned;
    if (n > 0 && (size_t)n <= len) {
        return n;
    }
    if (n == 0) {
        return on_zero;
    }
    if (n == -1) {
        int error = sceNetInetGetErrno();
        if (error == EAGAIN || error == EWOULDBLOCK || error == EINTR) {
            return MUSE_PORT_TIMEOUT;
        }
    }
    return MUSE_PORT_ERROR;
}

int muse_port_recv(muse_sock sock, void *buf, size_t len, uint32_t timeout_ms)
{
    int fd = (int)sock;
    if (!len) {
        return MUSE_PORT_ERROR;
    }
    if (len > INT_MAX) {
        len = INT_MAX;
    }
    int ready = wait_ready(fd, SCE_NET_INET_POLLIN, timeout_ms);
    if (ready <= 0) {
        return ready == 0 ? MUSE_PORT_TIMEOUT : MUSE_PORT_ERROR;
    }
    return checked_count(sceNetInetRecv(fd, buf, len, 0), len, MUSE_PORT_CLOSED);
}

int muse_port_send(muse_sock sock, const void *buf, size_t len, uint32_t timeout_ms)
{
    int fd = (int)sock;
    if (!len) {
        return MUSE_PORT_ERROR;
    }
    if (len > INT_MAX) {
        len = INT_MAX;
    }
    int ready = wait_ready(fd, SCE_NET_INET_POLLOUT, timeout_ms);
    if (ready <= 0) {
        return ready == 0 ? MUSE_PORT_TIMEOUT : MUSE_PORT_ERROR;
    }
    return checked_count(sceNetInetSend(fd, buf, len, 0), len, MUSE_PORT_TIMEOUT);
}

void muse_port_close(muse_sock sock)
{
    sceNetInetClose((int)sock);
}

/* The app must have called sceNetResolverInit() once, after sceNetInetInit(),
 * as part of the network bring-up it owns. Without it every name fails here
 * with MUSE_PORT_RESOLVE; dotted quads still work. */
/* The last address each host resolved to. A lookup is one small packet each
 * way, and on a real PSP one going missing stalled a request for as long as
 * the resolver was told to wait, so a recent answer is reused. */
static struct {
    char host[64];
    struct in_addr address;
    uint32_t at_ms;
    int valid;
} known[KNOWN_HOSTS];

static int known_slot(const char *host)
{
    for (int i = 0; i < KNOWN_HOSTS; i++) {
        if (known[i].valid && strcmp(known[i].host, host) == 0) {
            return i;
        }
    }
    return -1;
}

static void forget(const char *host)
{
    int slot = known_slot(host);
    if (slot >= 0) {
        known[slot].valid = 0;
    }
}

/* One question to one name server.
 *
 * The PSP's own resolver is not used: on a real PSP it sometimes never came
 * back from a lookup, timeout or not, and the thread that called it was lost
 * for good. Everything here waits on a poll with a limit. */
static int ask_server(int fd, const struct in_addr *server, const char *host, uint16_t id,
                      uint32_t wait_ms, struct in_addr *address)
{
    uint8_t packet[512];
    size_t n = muse_dns_query(packet, sizeof(packet), host, id);
    struct sockaddr_in peer;
    memset(&peer, 0, sizeof(peer));
    peer.sin_len = sizeof(peer);
    peer.sin_family = AF_INET;
    peer.sin_port = htons(53);
    peer.sin_addr = *server;
    if (n == 0 || sceNetInetSendto(fd, packet, n, 0, (const struct sockaddr *)&peer, sizeof(peer)) != n) {
        return -1;
    }
    if (wait_ready(fd, SCE_NET_INET_POLLIN, wait_ms) <= 0) {
        return -1;
    }
    socklen_t peer_size = sizeof(peer);
    size_t got = sceNetInetRecvfrom(fd, packet, sizeof(packet), 0, (struct sockaddr *)&peer, &peer_size);
    uint8_t found[4];
    if (got > sizeof(packet) || muse_dns_answer(packet, got, id, found) != MUSE_OK) {
        return -1;
    }
    memcpy(address, found, sizeof(found));
    return 0;
}

static int lookup(const char *host, uint32_t timeout_ms, struct in_addr *address)
{
    /* The network's own servers first, then two public ones. */
    struct in_addr servers[4];
    int count = 0;
    union SceNetApctlInfo info;
    if (sceNetApctlGetInfo(PSP_NET_APCTL_INFO_PRIMDNS, &info) == 0) {
        info.primaryDns[sizeof(info.primaryDns) - 1] = 0;
        count += sceNetInetInetAton(info.primaryDns, &servers[count]) != 0;
    }
    if (sceNetApctlGetInfo(PSP_NET_APCTL_INFO_SECDNS, &info) == 0) {
        info.secondaryDns[sizeof(info.secondaryDns) - 1] = 0;
        count += sceNetInetInetAton(info.secondaryDns, &servers[count]) != 0;
    }
    count += sceNetInetInetAton("1.1.1.1", &servers[count]) != 0;
    count += sceNetInetInetAton("8.8.8.8", &servers[count]) != 0;

    int fd = sceNetInetSocket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) {
        return -1;
    }
    uint32_t started = muse_port_now_ms();
    int rc = -1;
    for (int attempt = 0; rc != 0 && muse_port_now_ms() - started + LOOKUP_WAIT_MS <= timeout_ms;
         attempt++) {
        uint16_t id = 0;
        muse_port_random(&id, sizeof(id));
        rc = ask_server(fd, &servers[attempt % count], host, id, LOOKUP_WAIT_MS, address);
    }
    sceNetInetClose(fd);
    return rc;
}

static int resolve(const char *host, uint32_t timeout_ms, struct in_addr *address)
{
    int slot = known_slot(host);
    if (slot >= 0 && muse_port_now_ms() - known[slot].at_ms < KNOWN_HOST_MS) {
        *address = known[slot].address;
        return 0;
    }
    /* Leaves the rest of the caller's time for the connection itself. */
    int rc = lookup(host, timeout_ms > 6000 ? 6000 : timeout_ms, address);
    if (rc < 0) {
        /* A stale address beats none: the hosts this talks to rarely move. */
        if (slot >= 0) {
            *address = known[slot].address;
            return 0;
        }
        return -1;
    }
    if (slot < 0 && strlen(host) < sizeof(known[0].host)) {
        static int next;
        slot = next++ % KNOWN_HOSTS;
        strcpy(known[slot].host, host);
    }
    if (slot >= 0) {
        known[slot].address = *address;
        known[slot].at_ms = muse_port_now_ms();
        known[slot].valid = 1;
    }
    return 0;
}

static int connect_within(int fd, const struct sockaddr_in *peer, uint32_t timeout_ms)
{
    int on = 1;
    if (sceNetInetSetsockopt(fd, SOL_SOCKET, SO_NONBLOCK, &on, sizeof(on)) < 0) {
        return MUSE_PORT_ERROR;
    }
    if (sceNetInetConnect(fd, (const struct sockaddr *)peer, sizeof(*peer)) >= 0) {
        return 0;
    }
    if (sceNetInetGetErrno() != EINPROGRESS) {
        return MUSE_PORT_REFUSED;
    }
    if (wait_ready(fd, SCE_NET_INET_POLLOUT, timeout_ms) <= 0) {
        return MUSE_PORT_REFUSED;
    }
    int failure = 0;
    socklen_t size = sizeof(failure);
    if (sceNetInetGetsockopt(fd, SOL_SOCKET, SO_ERROR, &failure, &size) < 0 || failure != 0) {
        return MUSE_PORT_REFUSED;
    }
    return 0;
}

int muse_port_connect(const char *host, uint16_t port, uint32_t timeout_ms, muse_sock *out)
{
    struct sockaddr_in peer;
    memset(&peer, 0, sizeof(peer));
    peer.sin_len = sizeof(peer);
    peer.sin_family = AF_INET;
    peer.sin_port = htons(port);

    uint32_t started = sceKernelGetSystemTimeLow();
    if (!sceNetInetInetAton(host, &peer.sin_addr) && resolve(host, timeout_ms, &peer.sin_addr) != 0) {
        return MUSE_PORT_RESOLVE;
    }
    uint32_t spent_ms = (sceKernelGetSystemTimeLow() - started) / 1000;

    int fd = sceNetInetSocket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return MUSE_PORT_ERROR;
    }
    int rc = connect_within(fd, &peer, spent_ms < timeout_ms ? timeout_ms - spent_ms : 0);
    if (rc != 0) {
        sceNetInetClose(fd);
        forget(host);
        return rc;
    }
    *out = fd;
    return 0;
}

#else

/* ISO C forbids an empty translation unit, and the library builds with
 * -Wpedantic -Werror. */
typedef int muse_port_psp_unused;

#endif
