#if defined(__APPLE__)
#define _DARWIN_C_SOURCE
#else
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE
#endif

#include "muse_port.h"

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netdb.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#ifdef MSG_NOSIGNAL
#define MUSE_SEND_FLAGS (MSG_NOSIGNAL | MSG_DONTWAIT)
#else
#define MUSE_SEND_FLAGS MSG_DONTWAIT
#endif

struct muse_mutex {
    pthread_mutex_t handle;
};

muse_mutex *muse_port_mutex_new(void)
{
    muse_mutex *mutex = malloc(sizeof(*mutex));
    if (mutex != NULL && pthread_mutex_init(&mutex->handle, NULL) != 0) {
        free(mutex);
        mutex = NULL;
    }
    return mutex;
}

void muse_port_mutex_free(muse_mutex *mutex)
{
    if (mutex != NULL) {
        pthread_mutex_destroy(&mutex->handle);
        free(mutex);
    }
}

void muse_port_mutex_lock(muse_mutex *mutex)
{
    pthread_mutex_lock(&mutex->handle);
}

void muse_port_mutex_unlock(muse_mutex *mutex)
{
    pthread_mutex_unlock(&mutex->handle);
}

uint32_t muse_port_now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint32_t)((uint64_t)ts.tv_sec * 1000u + (uint64_t)ts.tv_nsec / 1000000u);
}

void muse_port_sleep_ms(uint32_t ms)
{
    struct timespec ts;
    ts.tv_sec = (time_t)(ms / 1000u);
    ts.tv_nsec = (long)(ms % 1000u) * 1000000L;
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

int64_t muse_port_unix_time(void)
{
    return (int64_t)time(NULL);
}

int muse_port_random(void *buf, size_t len)
{
#if defined(__APPLE__)
    arc4random_buf(buf, len);
    return 0;
#else
    unsigned char *p = buf;
    while (len > 0) {
        size_t n = len < 256 ? len : 256;
        if (getentropy(p, n) != 0) {
            return -1;
        }
        p += n;
        len -= n;
    }
    return 0;
#endif
}

static uint32_t ms_left(uint32_t deadline_ms)
{
    int32_t left = (int32_t)(deadline_ms - muse_port_now_ms());
    return left > 0 ? (uint32_t)left : 0;
}

/* 1 ready, 0 timeout, -1 error. */
static int wait_for(int fd, short events, uint32_t deadline_ms)
{
    for (;;) {
        struct pollfd pfd;
        pfd.fd = fd;
        pfd.events = events;
        pfd.revents = 0;
        uint32_t left = ms_left(deadline_ms);
        int rc = poll(&pfd, 1, left > (uint32_t)INT_MAX ? INT_MAX : (int)left);
        if (rc > 0) {
            return 1;
        }
        if (rc == 0) {
            return 0;
        }
        if (errno != EINTR) {
            return -1;
        }
    }
}

static int set_nonblocking(int fd, int on)
{
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0) {
        return -1;
    }
    flags = on ? (flags | O_NONBLOCK) : (flags & ~O_NONBLOCK);
    return fcntl(fd, F_SETFL, flags);
}

static int connect_one(const struct addrinfo *ai, uint32_t deadline_ms)
{
    int fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
    if (fd < 0) {
        return -1;
    }
#ifdef SO_NOSIGPIPE
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one));
#endif
    if (set_nonblocking(fd, 1) != 0) {
        close(fd);
        return -1;
    }
    int rc;
    do {
        rc = connect(fd, ai->ai_addr, ai->ai_addrlen);
    } while (rc != 0 && errno == EINTR);
    if (rc != 0 && errno == EINPROGRESS) {
        int err = 0;
        socklen_t err_len = sizeof(err);
        if (wait_for(fd, POLLOUT, deadline_ms) == 1 &&
            getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &err_len) == 0 && err == 0) {
            rc = 0;
        }
    }
    if (rc != 0 || set_nonblocking(fd, 0) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

int muse_port_connect(const char *host, uint16_t port, uint32_t timeout_ms, muse_sock *out)
{
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    char service[8];
    snprintf(service, sizeof(service), "%u", (unsigned)port);

    struct addrinfo hints = {0};
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    struct addrinfo *list = NULL;
    if (getaddrinfo(host, service, &hints, &list) != 0 || list == NULL) {
        return MUSE_PORT_RESOLVE;
    }
    int fd = -1;
    for (const struct addrinfo *ai = list; ai != NULL && fd < 0; ai = ai->ai_next) {
        fd = connect_one(ai, deadline_ms);
    }
    freeaddrinfo(list);
    if (fd < 0) {
        return MUSE_PORT_REFUSED;
    }
    *out = (muse_sock)fd;
    return 0;
}

static size_t clamp_len(size_t len)
{
    return len > (size_t)INT_MAX ? (size_t)INT_MAX : len;
}

int muse_port_recv(muse_sock sock, void *buf, size_t len, uint32_t timeout_ms)
{
    int fd = (int)sock;
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    for (;;) {
        int ready = wait_for(fd, POLLIN, deadline_ms);
        if (ready <= 0) {
            return ready == 0 ? MUSE_PORT_TIMEOUT : MUSE_PORT_ERROR;
        }
        ssize_t n = recv(fd, buf, clamp_len(len), MSG_DONTWAIT);
        if (n > 0) {
            return (int)n;
        }
        if (n == 0) {
            return MUSE_PORT_CLOSED;
        }
        if (errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
            return MUSE_PORT_ERROR;
        }
    }
}

int muse_port_send(muse_sock sock, const void *buf, size_t len, uint32_t timeout_ms)
{
    int fd = (int)sock;
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    for (;;) {
        int ready = wait_for(fd, POLLOUT, deadline_ms);
        if (ready <= 0) {
            return ready == 0 ? MUSE_PORT_TIMEOUT : MUSE_PORT_ERROR;
        }
        /* MSG_DONTWAIT keeps a large write from blocking past the timeout
         * once poll has reported room for only part of it. */
        ssize_t n = send(fd, buf, clamp_len(len), MUSE_SEND_FLAGS);
        if (n > 0) {
            return (int)n;
        }
        if (errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
            return MUSE_PORT_ERROR;
        }
    }
}

void muse_port_close(muse_sock sock)
{
    close((int)sock);
}
