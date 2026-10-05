#include "muse_net.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int append_header(muse_buf *b, const char *name, const char *value)
{
    int rc = muse_buf_append_str(b, name);
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, ": ");
    }
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, value);
    }
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, "\r\n");
    }
    return rc;
}

static int append_headers(muse_buf *b, const muse_header *headers, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        int rc = append_header(b, headers[i].name, headers[i].value);
        if (rc != MUSE_OK) {
            return rc;
        }
    }
    return MUSE_OK;
}

static int build_head(muse_buf *b, const char *method, const muse_url *url,
                      const muse_header *fixed, size_t nfixed, const muse_header *headers,
                      size_t nheaders)
{
    char host[sizeof(url->host) + 8];
    if (url->port == (url->secure ? 443 : 80)) {
        snprintf(host, sizeof(host), "%s", url->host);
    } else {
        snprintf(host, sizeof(host), "%s:%u", url->host, (unsigned)url->port);
    }
    int rc = muse_buf_append_str(b, method);
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, " ");
    }
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, url->path);
    }
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, " HTTP/1.1\r\n");
    }
    if (rc == MUSE_OK) {
        rc = append_header(b, "Host", host);
    }
    if (rc == MUSE_OK) {
        rc = append_headers(b, fixed, nfixed);
    }
    if (rc == MUSE_OK) {
        rc = append_headers(b, headers, nheaders);
    }
    if (rc == MUSE_OK) {
        rc = muse_buf_append_str(b, "\r\n");
    }
    return rc;
}

int muse_http_start(muse_tls *tls, bool allow_plaintext, const char *method, const char *url,
                    const muse_header *fixed, size_t nfixed, const muse_header *headers,
                    size_t nheaders, uint32_t deadline_ms, muse_stream **out)
{
    *out = NULL;
    muse_url *parsed = malloc(sizeof(*parsed));
    if (parsed == NULL) {
        return MUSE_ENOMEM;
    }
    muse_buf head;
    muse_buf_init(&head, MUSE_MAX_HTTP_HEAD);
    muse_stream *s = NULL;

    int rc = muse_url_parse(url, parsed);
    if (rc == MUSE_OK && !parsed->secure && !allow_plaintext) {
        rc = MUSE_EINVAL;
    }
    if (rc == MUSE_OK) {
        rc = build_head(&head, method, parsed, fixed, nfixed, headers, nheaders);
    }
    if (rc == MUSE_OK) {
        rc = muse_stream_open(tls, parsed->secure, parsed->host, parsed->port,
                              muse_ms_left(deadline_ms), &s);
    }
    if (rc == MUSE_OK) {
        rc = muse_stream_write_all(s, head.data, head.len, muse_ms_left(deadline_ms));
    }
    muse_buf_free(&head);
    free(parsed);
    if (rc != MUSE_OK) {
        muse_stream_close(s);
        return rc;
    }
    *out = s;
    return MUSE_OK;
}

static bool find_head_end(const muse_buf *rx, size_t *end)
{
    for (size_t i = 0; i < rx->len; i++) {
        if (rx->data[i] != '\n') {
            continue;
        }
        if (i + 1 < rx->len && rx->data[i + 1] == '\n') {
            *end = i + 2;
            return true;
        }
        if (i + 2 < rx->len && rx->data[i + 1] == '\r' && rx->data[i + 2] == '\n') {
            *end = i + 3;
            return true;
        }
    }
    return false;
}

static bool is_space(char c)
{
    return c == ' ' || c == '\t' || c == '\r' || c == '\n';
}

static bool is_digit(char c)
{
    return c >= '0' && c <= '9';
}

/* "HTTP/x.y <3 digits>[ reason]" */
static int parse_status_line(const char *line, size_t len)
{
    if (len < 5 || memcmp(line, "HTTP/", 5) != 0) {
        return MUSE_EPROTO;
    }
    size_t i = 5;
    while (i < len && !is_space(line[i])) {
        i++;
    }
    while (i < len && is_space(line[i])) {
        i++;
    }
    if (i + 3 > len || !is_digit(line[i]) || !is_digit(line[i + 1]) || !is_digit(line[i + 2])) {
        return MUSE_EPROTO;
    }
    if (i + 3 < len && !is_space(line[i + 3])) {
        return MUSE_EPROTO;
    }
    return (line[i] - '0') * 100 + (line[i + 1] - '0') * 10 + (line[i + 2] - '0');
}

/* Rewrites text in place into name\0value\0 pairs. A pair is never longer
 * than the "name:value\n" line it came from, so the write cursor stays
 * behind the read cursor. */
static int parse_head(char *text, size_t len, muse_http_head *head)
{
    if (memchr(text, '\0', len) != NULL) {
        return MUSE_EPROTO;
    }
    char *end = text + len;
    char *line = text;
    char *write = text;
    bool first = true;
    while (line < end) {
        char *eol = memchr(line, '\n', (size_t)(end - line));
        char *next = eol ? eol + 1 : end;
        size_t line_len = (size_t)((eol ? eol : end) - line);
        if (first) {
            int status = parse_status_line(line, line_len);
            if (status < 0) {
                return status;
            }
            head->status = status;
            first = false;
            line = next;
            continue;
        }
        char *colon = memchr(line, ':', line_len);
        if (colon != NULL) {
            char *name = line;
            char *name_end = colon;
            char *value = colon + 1;
            char *value_end = line + line_len;
            while (name < name_end && is_space(*name)) {
                name++;
            }
            while (name_end > name && is_space(name_end[-1])) {
                name_end--;
            }
            while (value < value_end && is_space(*value)) {
                value++;
            }
            while (value_end > value && is_space(value_end[-1])) {
                value_end--;
            }
            size_t name_len = (size_t)(name_end - name);
            size_t value_len = (size_t)(value_end - value);
            memmove(write, name, name_len);
            for (size_t i = 0; i < name_len; i++) {
                if (write[i] >= 'A' && write[i] <= 'Z') {
                    write[i] = (char)(write[i] - 'A' + 'a');
                }
            }
            write[name_len] = '\0';
            write += name_len + 1;
            memmove(write, value, value_len);
            write[value_len] = '\0';
            write += value_len + 1;
        }
        line = next;
    }
    if (first) {
        return MUSE_EPROTO;
    }
    head->fields = text;
    head->fields_len = (size_t)(write - text);
    return MUSE_OK;
}

int muse_http_read_head(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, muse_http_head *head)
{
    head->status = 0;
    head->fields = NULL;
    head->fields_len = 0;

    size_t end = 0;
    while (!find_head_end(rx, &end)) {
        if (rx->len >= MUSE_MAX_HTTP_HEAD) {
            return MUSE_EPROTO;
        }
        int n = muse_stream_fill(s, rx, muse_ms_left(deadline_ms));
        if (n == 0) {
            return MUSE_ETIMEOUT;
        }
        if (n == MUSE_EIO) {
            /* net.py treats EOF as the end of the head; only a missing
             * status line is an error. */
            end = rx->len;
            break;
        }
        if (n < 0) {
            return n;
        }
    }
    if (end > MUSE_MAX_HTTP_HEAD) {
        return MUSE_EPROTO;
    }
    char *text = malloc(end + 1);
    if (text == NULL) {
        return MUSE_ENOMEM;
    }
    if (end > 0) {
        memcpy(text, rx->data, end);
    }
    text[end] = '\0';
    int rc = parse_head(text, end, head);
    if (rc != MUSE_OK) {
        free(text);
        return rc;
    }
    muse_buf_consume(rx, end);
    return MUSE_OK;
}

const char *muse_http_head_get(const muse_http_head *head, const char *lower_name)
{
    const char *found = NULL;
    const char *p = head->fields;
    const char *end = p + head->fields_len;
    while (p < end) {
        const char *value = p + strlen(p) + 1;
        if (strcmp(p, lower_name) == 0) {
            found = value;
        }
        p = value + strlen(value) + 1;
    }
    return found;
}

void muse_http_head_free(muse_http_head *head)
{
    free(head->fields);
    head->fields = NULL;
    head->fields_len = 0;
}

static int pull(muse_stream *s, muse_buf *rx, uint32_t deadline_ms)
{
    int n = muse_stream_fill(s, rx, muse_ms_left(deadline_ms));
    if (n == 0) {
        return MUSE_ETIMEOUT;
    }
    return n < 0 ? n : MUSE_OK;
}

static int read_exact(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, size_t need,
                      muse_buf *out)
{
    while (need > 0) {
        if (rx->len == 0) {
            int rc = pull(s, rx, deadline_ms);
            if (rc != MUSE_OK) {
                return rc;
            }
        }
        size_t take = rx->len < need ? rx->len : need;
        int rc = muse_buf_append(out, rx->data, take);
        if (rc != MUSE_OK) {
            return rc;
        }
        muse_buf_consume(rx, take);
        need -= take;
    }
    return MUSE_OK;
}

/* Copies one line without its terminator and consumes it. */
static int read_line(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, char *line,
                     size_t line_cap)
{
    for (;;) {
        const uint8_t *eol = rx->len ? memchr(rx->data, '\n', rx->len) : NULL;
        if (eol != NULL) {
            size_t n = (size_t)(eol - rx->data);
            if (n >= line_cap) {
                return MUSE_EPROTO;
            }
            memcpy(line, rx->data, n);
            line[n] = '\0';
            muse_buf_consume(rx, n + 1);
            return MUSE_OK;
        }
        if (rx->len >= line_cap) {
            return MUSE_EPROTO;
        }
        int rc = pull(s, rx, deadline_ms);
        if (rc != MUSE_OK) {
            return rc;
        }
    }
}

/* Hex chunk size, ignoring extensions after ';'. An empty line counts as 0,
 * as in net.py. */
static int parse_chunk_size(const char *line, size_t *size)
{
    size_t value = 0;
    const char *p = line;
    while (is_space(*p)) {
        p++;
    }
    for (; *p != '\0' && *p != ';' && !is_space(*p); p++) {
        unsigned digit;
        if (is_digit(*p)) {
            digit = (unsigned)(*p - '0');
        } else if (*p >= 'a' && *p <= 'f') {
            digit = (unsigned)(*p - 'a' + 10);
        } else if (*p >= 'A' && *p <= 'F') {
            digit = (unsigned)(*p - 'A' + 10);
        } else {
            return MUSE_EPROTO;
        }
        value = value * 16 + digit;
        if (value > MUSE_MAX_HTTP_BODY) {
            return MUSE_ETOOBIG;
        }
    }
    *size = value;
    return MUSE_OK;
}

static int parse_content_length(const char *text, size_t *length)
{
    size_t value = 0;
    if (!is_digit(*text)) {
        return MUSE_EPROTO;
    }
    for (; *text != '\0'; text++) {
        if (!is_digit(*text)) {
            return MUSE_EPROTO;
        }
        value = value * 10 + (size_t)(*text - '0');
        if (value > MUSE_MAX_HTTP_BODY) {
            return MUSE_ETOOBIG;
        }
    }
    *length = value;
    return MUSE_OK;
}

static bool equals_ignore_case(const char *a, const char *b)
{
    for (; *a != '\0' && *b != '\0'; a++, b++) {
        char ca = (*a >= 'A' && *a <= 'Z') ? (char)(*a - 'A' + 'a') : *a;
        if (ca != *b) {
            return false;
        }
    }
    return *a == *b;
}

static int read_chunked(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, muse_buf *out)
{
    size_t total = 0;
    char line[64];
    for (;;) {
        size_t size = 0;
        int rc = read_line(s, rx, deadline_ms, line, sizeof(line));
        if (rc == MUSE_OK) {
            rc = parse_chunk_size(line, &size);
        }
        if (rc != MUSE_OK || size == 0) {
            return rc;
        }
        if (total > out->max || size > out->max - total) {
            return MUSE_ETOOBIG;
        }
        total += size;
        rc = read_exact(s, rx, deadline_ms, size, out);
        if (rc == MUSE_OK) {
            rc = read_line(s, rx, deadline_ms, line, sizeof(line));
        }
        if (rc != MUSE_OK) {
            return rc;
        }
    }
}

static int read_to_close(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, muse_buf *out)
{
    size_t total = 0;
    for (;;) {
        if (rx->len > MUSE_MAX_HTTP_BODY - total) {
            return MUSE_ETOOBIG;
        }
        total += rx->len;
        int rc = muse_buf_append(out, rx->data, rx->len);
        if (rc != MUSE_OK) {
            return rc;
        }
        muse_buf_clear(rx);
        rc = pull(s, rx, deadline_ms);
        if (rc == MUSE_EIO) {
            return MUSE_OK;
        }
        if (rc != MUSE_OK) {
            return rc;
        }
    }
}

static int read_body(muse_stream *s, muse_buf *rx, uint32_t deadline_ms,
                     const muse_http_head *head, muse_buf *out)
{
    const char *encoding = muse_http_head_get(head, "transfer-encoding");
    if (encoding != NULL && equals_ignore_case(encoding, "chunked")) {
        return read_chunked(s, rx, deadline_ms, out);
    }
    const char *length_text = muse_http_head_get(head, "content-length");
    if (length_text != NULL) {
        size_t length = 0;
        int rc = parse_content_length(length_text, &length);
        if (rc != MUSE_OK) {
            return rc;
        }
        return read_exact(s, rx, deadline_ms, length, out);
    }
    return read_to_close(s, rx, deadline_ms, out);
}

int muse_http_request(muse_tls *tls, bool allow_plaintext, const char *method, const char *url,
                      const muse_header *headers, size_t nheaders, const void *body,
                      size_t body_len, uint32_t timeout_ms, int *status, muse_buf *body_out)
{
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    *status = 0;

    char length_text[24];
    muse_header fixed[2] = {{"Connection", "close"}, {"Content-Length", length_text}};
    snprintf(length_text, sizeof(length_text), "%lu", (unsigned long)body_len);

    muse_stream *s = NULL;
    int rc = muse_http_start(tls, allow_plaintext, method, url, fixed, body != NULL ? 2 : 1,
                             headers, nheaders, deadline_ms, &s);
    if (rc != MUSE_OK) {
        return rc;
    }
    if (body != NULL && body_len > 0) {
        rc = muse_stream_write_all(s, body, body_len, muse_ms_left(deadline_ms));
    }

    muse_buf rx;
    muse_buf_init(&rx, MUSE_RX_MAX);
    muse_http_head head = {0, NULL, 0};
    if (rc == MUSE_OK) {
        rc = muse_http_read_head(s, &rx, deadline_ms, &head);
    }
    if (rc == MUSE_OK) {
        rc = read_body(s, &rx, deadline_ms, &head, body_out);
    }
    if (rc == MUSE_OK) {
        *status = head.status;
    }
    muse_http_head_free(&head);
    muse_buf_free(&rx);
    muse_stream_close(s);
    return rc;
}

static void log_to_stderr(void *user, const char *line)
{
    (void)user;
    fprintf(stderr, "%s\n", line);
}

int muse_https_get(const char *url, const char *user_agent, bool ignore_cert_dates, int *status,
                   char **body)
{
    if (url == NULL || status == NULL || body == NULL) {
        return MUSE_EINVAL;
    }
    *status = 0;
    *body = NULL;
    muse_tls *tls = malloc(sizeof(*tls));
    if (tls == NULL) {
        return MUSE_ENOMEM;
    }
    muse_log log = {NULL, log_to_stderr};
    muse_buf out;
    muse_buf_init(&out, MUSE_MAX_HTTP_BODY);

    int rc = muse_tls_init(tls, ignore_cert_dates, log);
    if (rc == MUSE_OK) {
        muse_header agent = {"User-Agent", user_agent};
        rc = muse_http_request(tls, false, "GET", url, &agent, user_agent != NULL ? 1 : 0, NULL,
                               0, MUSE_HTTP_TIMEOUT_MS, status, &out);
    }
    if (rc == MUSE_OK) {
        *body = muse_buf_take_str(&out);
        if (*body == NULL) {
            rc = MUSE_ENOMEM;
        }
    }
    muse_buf_free(&out);
    muse_tls_free(tls);
    free(tls);
    return rc;
}

/* Where a streamed response body goes, piece by piece. */
typedef struct body_sink {
    int (*fn)(void *user, int status, const void *data, size_t len);
    void *user;
    int status;
} body_sink;

static int sink_exact(muse_stream *s, muse_buf *rx, uint32_t deadline_ms, size_t need,
                      body_sink *sink)
{
    while (need > 0) {
        if (rx->len == 0) {
            int rc = pull(s, rx, deadline_ms);
            if (rc != MUSE_OK) {
                return rc;
            }
        }
        size_t take = rx->len < need ? rx->len : need;
        if (sink->fn(sink->user, sink->status, rx->data, take) != 0) {
            return MUSE_ETOOBIG;
        }
        muse_buf_consume(rx, take);
        need -= take;
    }
    return MUSE_OK;
}

static int sink_body(muse_stream *s, muse_buf *rx, uint32_t deadline_ms,
                     const muse_http_head *head, body_sink *sink)
{
    const char *encoding = muse_http_head_get(head, "transfer-encoding");
    if (encoding != NULL && equals_ignore_case(encoding, "chunked")) {
        char line[64];
        for (;;) {
            size_t size = 0;
            int rc = read_line(s, rx, deadline_ms, line, sizeof(line));
            if (rc == MUSE_OK) {
                rc = parse_chunk_size(line, &size);
            }
            if (rc != MUSE_OK || size == 0) {
                return rc;
            }
            rc = sink_exact(s, rx, deadline_ms, size, sink);
            if (rc == MUSE_OK) {
                rc = read_line(s, rx, deadline_ms, line, sizeof(line));
            }
            if (rc != MUSE_OK) {
                return rc;
            }
        }
    }
    const char *length_text = muse_http_head_get(head, "content-length");
    if (length_text != NULL) {
        size_t length = 0;
        int rc = parse_content_length(length_text, &length);
        return rc != MUSE_OK ? rc : sink_exact(s, rx, deadline_ms, length, sink);
    }
    for (;;) {
        int rc = sink_exact(s, rx, deadline_ms, rx->len, sink);
        if (rc == MUSE_OK) {
            rc = pull(s, rx, deadline_ms);
        }
        if (rc != MUSE_OK) {
            return rc == MUSE_EIO ? MUSE_OK : rc;
        }
    }
}

struct muse_https {
    muse_tls tls;
    muse_stream *stream;
};

muse_https *muse_https_open(const char *url, uint32_t timeout_ms,
                            void (*log_fn)(void *user, const char *line), void *log_user)
{
    muse_https *c = calloc(1, sizeof(*c));
    muse_url *parsed = malloc(sizeof(*parsed));
    muse_log log = {log_user, log_fn};
    bool ready = c != NULL && parsed != NULL && url != NULL && muse_url_parse(url, parsed) == MUSE_OK
                 && parsed->secure && muse_tls_init(&c->tls, false, log) == MUSE_OK;
    if (ready) {
        ready = mbedtls_x509_crt_parse_der(&c->tls.root, muse_gts_root_r4_der,
                                           muse_gts_root_r4_der_len) == 0
                && muse_stream_open(&c->tls, true, parsed->host, parsed->port, timeout_ms,
                                    &c->stream) == MUSE_OK;
        if (!ready) {
            muse_tls_free(&c->tls);
        }
    }
    free(parsed);
    if (!ready) {
        free(c);
        return NULL;
    }
    return c;
}

void muse_https_close(muse_https *c)
{
    if (c == NULL) {
        return;
    }
    muse_stream_close(c->stream);
    muse_tls_free(&c->tls);
    free(c);
}

int muse_https_post(muse_https *c, const char *url, const char *bearer, const char *content_type,
                    const void *body, size_t body_len, uint32_t timeout_ms,
                    int (*on_data)(void *user, int status, const void *data, size_t len),
                    void *data_user, int *status)
{
    if (c == NULL || url == NULL || bearer == NULL || content_type == NULL || body == NULL
        || status == NULL || on_data == NULL) {
        muse_https_close(c);
        return MUSE_EINVAL;
    }
    *status = 0;
    uint32_t deadline_ms = muse_port_now_ms() + timeout_ms;
    size_t auth_size = strlen(bearer) + 8;
    char *auth = malloc(auth_size);
    muse_url *parsed = malloc(sizeof(*parsed));
    muse_buf head_out, rx;
    muse_buf_init(&head_out, MUSE_MAX_HTTP_HEAD);
    muse_buf_init(&rx, MUSE_RX_MAX);
    muse_http_head head = {0, NULL, 0};

    int rc = auth != NULL && parsed != NULL ? muse_url_parse(url, parsed) : MUSE_ENOMEM;
    if (rc == MUSE_OK) {
        char length_text[24];
        snprintf(auth, auth_size, "Bearer %s", bearer);
        snprintf(length_text, sizeof(length_text), "%lu", (unsigned long)body_len);
        muse_header fixed[2] = {{"Connection", "close"}, {"Content-Length", length_text}};
        muse_header headers[2] = {{"Authorization", auth}, {"Content-Type", content_type}};
        rc = build_head(&head_out, "POST", parsed, fixed, 2, headers, 2);
    }
    if (rc == MUSE_OK) {
        rc = muse_stream_write_all(c->stream, head_out.data, head_out.len,
                                   muse_ms_left(deadline_ms));
    }
    if (rc == MUSE_OK && body_len > 0) {
        rc = muse_stream_write_all(c->stream, body, body_len, muse_ms_left(deadline_ms));
    }
    if (rc == MUSE_OK) {
        rc = muse_http_read_head(c->stream, &rx, deadline_ms, &head);
    }
    if (rc == MUSE_OK) {
        *status = head.status;
        body_sink sink = {on_data, data_user, head.status};
        rc = sink_body(c->stream, &rx, deadline_ms, &head, &sink);
    }
    muse_http_head_free(&head);
    muse_buf_free(&rx);
    muse_buf_free(&head_out);
    free(parsed);
    free(auth);
    muse_https_close(c);
    return rc;
}
