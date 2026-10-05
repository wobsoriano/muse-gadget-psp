#include "muse_net.h"

#include <stdlib.h>
#include <string.h>

#include <mbedtls/base64.h>
#include <mbedtls/sha1.h>

#define WS_GUID "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
#define WS_MAX_CONTROL 125u

typedef struct ws_frame {
    bool active;      /* header parsed, payload still arriving */
    bool fin;
    bool masked;
    uint8_t opcode;
    uint8_t mask[4];
    size_t left;
    size_t done;
} ws_frame;

struct muse_ws {
    muse_stream *s;
    muse_buf rx;
    ws_frame frame;
    uint8_t control[WS_MAX_CONTROL];
    /* The message being assembled. Payload moves here as it arrives so rx
     * never holds more than one read. */
    muse_buf parts;
    int kind;         /* 0 when no message is open */
    uint32_t last_rx_ms;
};

static int write_frame(muse_ws *ws, int opcode, const void *payload, size_t len,
                       uint32_t timeout_ms)
{
    uint8_t head[14];
    size_t n = 2;
    head[0] = (uint8_t)(0x80 | (opcode & 0x0F));
    if (len < 126) {
        head[1] = (uint8_t)(0x80 | len);
    } else if (len < 65536) {
        head[1] = 0x80 | 126;
        head[2] = (uint8_t)(len >> 8);
        head[3] = (uint8_t)len;
        n = 4;
    } else {
        head[1] = 0x80 | 127;
        uint64_t wide = len;
        for (int i = 0; i < 8; i++) {
            head[2 + i] = (uint8_t)(wide >> (56 - 8 * i));
        }
        n = 10;
    }
    /* An all-zero mask leaves the payload unchanged, so no XOR pass and no
     * copy. Masking protects plaintext proxies from browsers; under TLS it
     * adds nothing. */
    memset(head + n, 0, 4);
    n += 4;
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    int rc = muse_stream_write_all(ws->s, head, n, timeout_ms);
    if (rc == MUSE_OK && len > 0) {
        rc = muse_stream_write_all(ws->s, payload, len, muse_ms_left(deadline_ms));
    }
    return rc;
}

static int make_key(muse_tls *tls, char key[25])
{
    unsigned char raw[16];
    int rc = tls != NULL ? muse_tls_random(tls, raw, sizeof(raw))
                         : (muse_port_random(raw, sizeof(raw)) == 0 ? MUSE_OK : MUSE_EIO);
    if (rc != MUSE_OK) {
        return rc;
    }
    size_t out_len = 0;
    if (mbedtls_base64_encode((unsigned char *)key, 25, &out_len, raw, sizeof(raw)) != 0) {
        return MUSE_EINVAL;
    }
    return MUSE_OK;
}

static bool accept_matches(const char *key, const char *accept)
{
    unsigned char input[24 + sizeof(WS_GUID)];
    unsigned char digest[20];
    char expected[32];
    size_t out_len = 0;
    memcpy(input, key, 24);
    memcpy(input + 24, WS_GUID, sizeof(WS_GUID));
    if (mbedtls_sha1_ret(input, 24 + sizeof(WS_GUID) - 1, digest) != 0 ||
        mbedtls_base64_encode((unsigned char *)expected, sizeof(expected), &out_len, digest,
                              sizeof(digest)) != 0) {
        return false;
    }
    return accept != NULL && strcmp(accept, expected) == 0;
}

int muse_ws_connect(muse_tls *tls, bool allow_plaintext, const char *url,
                    const muse_header *headers, size_t nheaders, uint32_t timeout_ms,
                    muse_ws **out)
{
    *out = NULL;
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    char key[25];
    int rc = make_key(tls, key);
    if (rc != MUSE_OK) {
        return rc;
    }
    muse_ws *ws = calloc(1, sizeof(*ws));
    if (ws == NULL) {
        return MUSE_ENOMEM;
    }
    muse_buf_init(&ws->rx, MUSE_RX_MAX);
    muse_buf_init(&ws->parts, MUSE_MAX_WS_MESSAGE);

    const muse_header fixed[] = {
        {"Upgrade", "websocket"},
        {"Connection", "Upgrade"},
        {"Sec-WebSocket-Key", key},
        {"Sec-WebSocket-Version", "13"},
    };
    muse_http_head head = {0, NULL, 0};
    rc = muse_http_start(tls, allow_plaintext, "GET", url, fixed,
                         sizeof(fixed) / sizeof(fixed[0]), headers, nheaders, deadline_ms,
                         &ws->s);
    if (rc == MUSE_OK) {
        rc = muse_http_read_head(ws->s, &ws->rx, deadline_ms, &head);
    }
    if (rc == MUSE_OK) {
        if (head.status == 401) {
            rc = MUSE_EAUTH;
        } else if (head.status == 403) {
            rc = MUSE_EFORBIDDEN;
        } else if (head.status != 101) {
            rc = MUSE_EHTTP;
        } else if (!accept_matches(key, muse_http_head_get(&head, "sec-websocket-accept"))) {
            rc = MUSE_EPROTO;
        }
    }
    muse_http_head_free(&head);
    if (rc != MUSE_OK) {
        muse_stream_close(ws->s);
        muse_buf_free(&ws->rx);
        free(ws);
        return rc;
    }
    ws->last_rx_ms = muse_port_now_ms();
    *out = ws;
    return MUSE_OK;
}

int muse_ws_send(muse_ws *ws, int opcode, const void *payload, size_t len)
{
    return write_frame(ws, opcode, payload, len, MUSE_WRITE_TIMEOUT_MS);
}

/* 1 header parsed and consumed, 0 need more bytes, MUSE_EPROTO. */
static int parse_frame_header(muse_ws *ws)
{
    const uint8_t *p = ws->rx.data;
    if (ws->rx.len < 2) {
        return 0;
    }
    ws_frame *f = &ws->frame;
    size_t short_len = p[1] & 0x7F;
    size_t len_bytes = short_len == 126 ? 2 : short_len == 127 ? 8 : 0;
    bool masked = (p[1] & 0x80) != 0;
    size_t head_len = 2 + len_bytes + (masked ? 4u : 0u);
    if (ws->rx.len < head_len) {
        return 0;
    }
    uint64_t len = short_len;
    if (len_bytes > 0) {
        len = 0;
        for (size_t i = 0; i < len_bytes; i++) {
            len = (len << 8) | p[2 + i];
        }
    }
    if (len > MUSE_MAX_WS_MESSAGE) {
        return MUSE_EPROTO;
    }

    f->fin = (p[0] & 0x80) != 0;
    f->opcode = p[0] & 0x0F;
    f->masked = false;
    if (masked) {
        memcpy(f->mask, p + 2 + len_bytes, 4);
        f->masked = (f->mask[0] | f->mask[1] | f->mask[2] | f->mask[3]) != 0;
    }
    f->left = (size_t)len;
    f->done = 0;

    switch (f->opcode) {
    case MUSE_WS_TEXT:
    case MUSE_WS_BINARY:
        ws->kind = f->opcode;
        muse_buf_clear(&ws->parts);
        break;
    case 0x0:
        if (ws->kind == 0) {
            return MUSE_EPROTO;
        }
        break;
    case MUSE_WS_CLOSE:
    case MUSE_WS_PING:
    case MUSE_WS_PONG:
        if (!f->fin || f->left > WS_MAX_CONTROL) {
            return MUSE_EPROTO;
        }
        break;
    default:
        return MUSE_EPROTO;
    }
    if (f->opcode < 0x8 && f->left > MUSE_MAX_WS_MESSAGE - ws->parts.len) {
        return MUSE_EPROTO;
    }
    muse_buf_consume(&ws->rx, head_len);
    f->active = true;
    return 1;
}

static int take_payload(muse_ws *ws)
{
    ws_frame *f = &ws->frame;
    size_t take = ws->rx.len < f->left ? ws->rx.len : f->left;
    if (take == 0) {
        return MUSE_OK;
    }
    uint8_t *dest;
    if (f->opcode >= 0x8) {
        dest = ws->control + f->done;
        memcpy(dest, ws->rx.data, take);
    } else {
        int rc = muse_buf_append(&ws->parts, ws->rx.data, take);
        if (rc != MUSE_OK) {
            return rc;
        }
        dest = ws->parts.data + ws->parts.len - take;
    }
    if (f->masked) {
        for (size_t i = 0; i < take; i++) {
            dest[i] ^= f->mask[(f->done + i) & 3];
        }
    }
    muse_buf_consume(&ws->rx, take);
    f->left -= take;
    f->done += take;
    return MUSE_OK;
}

static void hand_over(muse_ws *ws, muse_buf *msg)
{
    uint8_t *spare = msg->data;
    size_t spare_cap = msg->cap;
    msg->data = ws->parts.data;
    msg->len = ws->parts.len;
    msg->cap = ws->parts.cap;
    ws->parts.data = spare;
    ws->parts.len = 0;
    ws->parts.cap = spare_cap;
}

/* Advances over whatever rx holds: 1 message ready, 0 need more bytes. */
static int advance(muse_ws *ws, int *opcode, muse_buf *msg)
{
    ws_frame *f = &ws->frame;
    for (;;) {
        if (!f->active) {
            int rc = parse_frame_header(ws);
            if (rc <= 0) {
                return rc;
            }
        }
        int rc = take_payload(ws);
        if (rc != MUSE_OK) {
            return rc;
        }
        if (f->left > 0) {
            return 0;
        }
        f->active = false;
        ws->last_rx_ms = muse_port_now_ms();

        if (f->opcode == MUSE_WS_PING) {
            if (write_frame(ws, MUSE_WS_PONG, ws->control, f->done, MUSE_WRITE_TIMEOUT_MS) !=
                MUSE_OK) {
                return MUSE_EIO;
            }
        } else if (f->opcode == MUSE_WS_CLOSE) {
            return MUSE_EIO;
        } else if (f->opcode < 0x8 && f->fin) {
            if (ws->parts.len > msg->max) {
                return MUSE_ETOOBIG;
            }
            *opcode = ws->kind;
            ws->kind = 0;
            hand_over(ws, msg);
            return 1;
        }
    }
}

int muse_ws_recv(muse_ws *ws, uint32_t timeout_ms, int *opcode, muse_buf *msg)
{
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    for (;;) {
        int rc = advance(ws, opcode, msg);
        if (rc != 0) {
            return rc;
        }
        int n = muse_stream_fill(ws->s, &ws->rx, muse_ms_left(deadline_ms));
        if (n == 0) {
            return 0;
        }
        if (n < 0) {
            return n == MUSE_ENOMEM ? MUSE_ENOMEM : MUSE_EIO;
        }
    }
}

uint32_t muse_ws_last_rx_ms(const muse_ws *ws)
{
    return ws->last_rx_ms;
}

void muse_ws_close(muse_ws *ws)
{
    if (ws == NULL) {
        return;
    }
    write_frame(ws, MUSE_WS_CLOSE, NULL, 0, MUSE_CLOSE_TIMEOUT_MS);
    muse_stream_close(ws->s);
    muse_buf_free(&ws->rx);
    muse_buf_free(&ws->parts);
    free(ws);
}
