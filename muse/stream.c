#include "muse_net.h"

#include <limits.h>
#include <stdlib.h>

#include <mbedtls/error.h>
#include <mbedtls/net_sockets.h>

struct muse_stream {
    muse_sock sock;
    bool secure;
    mbedtls_ssl_context ssl;
    /* The ssl config is shared by every connection, so its read timeout
     * cannot carry a per-call value. The bio callbacks use this instead. */
    uint32_t deadline_ms;
    muse_log log;
};

static size_t clamp_len(size_t len)
{
    return len > (size_t)INT_MAX ? (size_t)INT_MAX : len;
}

static int bio_recv(void *ctx, unsigned char *buf, size_t len, uint32_t timeout)
{
    (void)timeout;
    muse_stream *s = ctx;
    int n = muse_port_recv(s->sock, buf, clamp_len(len), muse_ms_left(s->deadline_ms));
    if (n > 0) {
        return n;
    }
    if (n == MUSE_PORT_TIMEOUT) {
        return MBEDTLS_ERR_SSL_TIMEOUT;
    }
    if (n == MUSE_PORT_CLOSED) {
        return 0;
    }
    return MBEDTLS_ERR_NET_RECV_FAILED;
}

static int bio_send(void *ctx, const unsigned char *buf, size_t len)
{
    muse_stream *s = ctx;
    int n = muse_port_send(s->sock, buf, clamp_len(len), muse_ms_left(s->deadline_ms));
    if (n > 0) {
        return n;
    }
    if (n == MUSE_PORT_TIMEOUT) {
        return MBEDTLS_ERR_SSL_WANT_WRITE;
    }
    return MBEDTLS_ERR_NET_SEND_FAILED;
}

static bool wants_retry(int ret)
{
    return ret == MBEDTLS_ERR_SSL_WANT_READ || ret == MBEDTLS_ERR_SSL_WANT_WRITE;
}

static int handshake(muse_stream *s, const char *host)
{
    int ret;
    do {
        ret = mbedtls_ssl_handshake(&s->ssl);
    } while (wants_retry(ret) && muse_ms_left(s->deadline_ms) > 0);
    if (ret == 0) {
        muse_logf(&s->log, "tls: %s %s %s", host, mbedtls_ssl_get_version(&s->ssl),
                  mbedtls_ssl_get_ciphersuite(&s->ssl));
        return MUSE_OK;
    }

    char text[128];
    mbedtls_strerror(ret, text, sizeof(text));
    muse_logf(&s->log, "tls: handshake with %s failed: -0x%04x %s", host, (unsigned)-ret, text);
    uint32_t flags = mbedtls_ssl_get_verify_result(&s->ssl);
    if (flags != 0 && flags != 0xFFFFFFFFu) {
        char info[384];
        if (mbedtls_x509_crt_verify_info(info, sizeof(info), "", flags) > 0) {
            for (char *p = info; *p != '\0'; p++) {
                if (*p == '\n') {
                    *p = p[1] == '\0' ? '\0' : ' ';
                }
            }
            muse_logf(&s->log, "tls: certificate for %s rejected: %s", host, info);
        }
    }
    if (wants_retry(ret) || ret == MBEDTLS_ERR_SSL_TIMEOUT) {
        return MUSE_ETIMEOUT;
    }
    return MUSE_ETLS;
}

int muse_stream_open(muse_tls *tls, bool secure, const char *host, uint16_t port,
                     uint32_t timeout_ms, muse_stream **out)
{
    *out = NULL;
    if (host == NULL || (secure && tls == NULL)) {
        return MUSE_EINVAL;
    }
    muse_stream *s = calloc(1, sizeof(*s));
    if (s == NULL) {
        return MUSE_ENOMEM;
    }
    s->secure = secure;
    s->deadline_ms = muse_port_now_ms() + timeout_ms;
    if (tls != NULL) {
        s->log = tls->log;
    }

    muse_logf(&s->log, "connecting to %s", host);
    int rc = muse_port_connect(host, port, timeout_ms, &s->sock);
    if (rc != 0) {
        muse_logf(&s->log, "connect to %s:%u failed: %d", host, (unsigned)port, rc);
        free(s);
        return MUSE_EIO;
    }
    if (!secure) {
        *out = s;
        return MUSE_OK;
    }
    muse_logf(&s->log, "reached %s, starting the handshake", host);

    mbedtls_ssl_init(&s->ssl);
    rc = MUSE_ETLS;
    if (mbedtls_ssl_setup(&s->ssl, &tls->conf) == 0 &&
        mbedtls_ssl_set_hostname(&s->ssl, host) == 0) {
        mbedtls_ssl_set_bio(&s->ssl, s, bio_send, NULL, bio_recv);
        rc = handshake(s, host);
    }
    if (rc != MUSE_OK) {
        mbedtls_ssl_free(&s->ssl);
        muse_port_close(s->sock);
        free(s);
        return rc;
    }
    *out = s;
    return MUSE_OK;
}

int muse_stream_read(muse_stream *s, void *buf, size_t len, uint32_t timeout_ms)
{
    len = clamp_len(len);
    if (!s->secure) {
        int n = muse_port_recv(s->sock, buf, len, timeout_ms);
        if (n > 0) {
            return n;
        }
        return n == MUSE_PORT_TIMEOUT ? 0 : MUSE_EIO;
    }
    s->deadline_ms = muse_port_now_ms() + timeout_ms;
    for (;;) {
        int ret = mbedtls_ssl_read(&s->ssl, buf, len);
        if (ret > 0) {
            return ret;
        }
        if (ret == MBEDTLS_ERR_SSL_TIMEOUT) {
            return 0;
        }
        if (!wants_retry(ret)) {
            return MUSE_EIO;
        }
        if (muse_ms_left(s->deadline_ms) == 0) {
            return 0;
        }
    }
}

int muse_stream_write_all(muse_stream *s, const void *buf, size_t len, uint32_t timeout_ms)
{
    const unsigned char *p = buf;
    s->deadline_ms = muse_port_now_ms() + timeout_ms;
    while (len > 0) {
        int n;
        if (s->secure) {
            n = mbedtls_ssl_write(&s->ssl, p, len);
            if (wants_retry(n)) {
                n = 0;
            } else if (n <= 0) {
                return MUSE_EIO;
            }
        } else {
            n = muse_port_send(s->sock, p, clamp_len(len), muse_ms_left(s->deadline_ms));
            if (n < 0) {
                return MUSE_EIO;
            }
        }
        if (n == 0 && muse_ms_left(s->deadline_ms) == 0) {
            return MUSE_ETIMEOUT;
        }
        p += n;
        len -= (size_t)n;
    }
    return MUSE_OK;
}

void muse_stream_close(muse_stream *s)
{
    if (s == NULL) {
        return;
    }
    if (s->secure) {
        s->deadline_ms = muse_port_now_ms() + MUSE_CLOSE_TIMEOUT_MS;
        mbedtls_ssl_close_notify(&s->ssl);
        mbedtls_ssl_free(&s->ssl);
    }
    muse_port_close(s->sock);
    free(s);
}

int muse_stream_fill(muse_stream *s, muse_buf *rx, uint32_t timeout_ms)
{
    if (rx->len >= rx->max) {
        return MUSE_ETOOBIG;
    }
    size_t want = rx->max - rx->len;
    if (want > MUSE_READ_CHUNK) {
        want = MUSE_READ_CHUNK;
    }
    int rc = muse_buf_reserve(rx, rx->len + want);
    if (rc != MUSE_OK) {
        return rc;
    }
    int n = muse_stream_read(s, rx->data + rx->len, want, timeout_ms);
    if (n > 0) {
        rx->len += (size_t)n;
    }
    return n;
}
