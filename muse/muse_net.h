/* Shared by stream.c, http.c and ws.c only: deadlines, buffered reads and the
 * HTTP/1.1 head both the request client and the WebSocket upgrade speak. */
#ifndef MUSE_NET_H
#define MUSE_NET_H

#include "muse_internal.h"

#define MUSE_READ_CHUNK 4096u
#define MUSE_RX_MAX (MUSE_MAX_HTTP_HEAD + MUSE_READ_CHUNK)

static inline uint32_t muse_ms_left(uint32_t deadline_ms)
{
    int32_t left = (int32_t)(deadline_ms - muse_port_now_ms());
    return left > 0 ? (uint32_t)left : 0;
}

/* One read appended to rx: >0 bytes added, 0 timeout, MUSE_EIO closed,
 * MUSE_ETOOBIG when rx is already full, MUSE_ENOMEM. */
int muse_stream_fill(muse_stream *s, muse_buf *rx, uint32_t timeout_ms);

typedef struct muse_http_head {
    int status;
    char *fields;       /* name\0value\0 pairs, names lowercased */
    size_t fields_len;
} muse_http_head;

/* Parses url, opens the stream and sends the request head. `fixed` headers
 * go before the caller's. */
int muse_http_start(muse_tls *tls, bool allow_plaintext, const char *method, const char *url,
                    const muse_header *fixed, size_t nfixed, const muse_header *headers,
                    size_t nheaders, uint32_t deadline_ms, muse_stream **out);
/* Bytes past the head stay in rx. */
int muse_http_read_head(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, muse_http_head *head);
/* NULL when absent; the last occurrence wins, as in net.py's dict. */
const char *muse_http_head_get(const muse_http_head *head, const char *lower_name);
void muse_http_head_free(muse_http_head *head);

#endif
