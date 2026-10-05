/* Module contracts inside the library. Not installed. Every size cap the
 * wire could push against is named here so a reader can audit them in one
 * place. mbedTLS is the 2.28 API (the PSP toolchain and brew's mbedtls@2). */
#ifndef MUSE_INTERNAL_H
#define MUSE_INTERNAL_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "muse.h"
#include "muse_port.h"

#include <mbedtls/ctr_drbg.h>
#include <mbedtls/entropy.h>
#include <mbedtls/ssl.h>
#include <mbedtls/x509_crt.h>

/* Size caps. */
#define MUSE_MAX_HTTP_BODY (256u * 1024u)        /* api.py responses */
#define MUSE_MAX_HTTP_HEAD (8u * 1024u)
#define MUSE_MAX_WS_MESSAGE (4u * 1024u * 1024u) /* net.py MAX_WS_MESSAGE */
#define MUSE_MAX_RESPONSE_BYTES (1024u * 1024u)  /* link.py MAX_RESPONSE_BYTES */
#define MUSE_MAX_EVENT_LINE (256u * 1024u)       /* link.py MAX_EVENT_LINE */
#define MUSE_MAX_INBOUND_MESSAGE (4u * 1024u * 1024u)
#define MUSE_MAX_CHUNK_PAYLOAD 65489u             /* noise.py MAX_CHUNK_PAYLOAD */
#define MUSE_MAX_TOTAL_CHUNKS 256u
#define MUSE_MAX_PENDING_ASSEMBLIES 4u            /* 16 in Python; the PSP has less RAM */
#define MUSE_MAX_ASSEMBLY_BYTES (2u * 1024u * 1024u)
#define MUSE_ASSEMBLY_TTL_MS 60000u
#define MUSE_MAX_REPLY_TEXT (64u * 1024u)
#define MUSE_MAX_VOICE_WAV (600u * 1024u)
#define MUSE_BODY_CHUNK (16u * 1024u)             /* voice note body pieces */

/* Timeouts, ms. */
#define MUSE_HTTP_TIMEOUT_MS 15000u
#define MUSE_WS_CONNECT_TIMEOUT_MS 20000u
#define MUSE_HANDSHAKE_TIMEOUT_MS 20000u
#define MUSE_REQUEST_TIMEOUT_MS 60000u
#define MUSE_FIRST_REPLY_MS 300000u
#define MUSE_REPLY_QUIET_MS 3000u
#define MUSE_REPLY_SETTLED_MS 350u
#define MUSE_TURN_CAP_MS 900000u
#define MUSE_PING_INTERVAL_MS 15000u
#define MUSE_PING_TIMEOUT_MS 10000u
#define MUSE_RX_TIMEOUT_MS 45000u
#define MUSE_WRITE_TIMEOUT_MS 10000u
#define MUSE_READ_SLICE_MS 250u                   /* one pass of the serve loop */
#define MUSE_CLOSE_TIMEOUT_MS 2000u

/* --- buf.c: growable byte buffer with a hard cap --------------------------- */
typedef struct muse_buf {
    uint8_t *data;
    size_t len;
    size_t cap;
    size_t max;   /* append fails with MUSE_ETOOBIG past this */
} muse_buf;

void muse_buf_init(muse_buf *b, size_t max);
int muse_buf_reserve(muse_buf *b, size_t total);
int muse_buf_append(muse_buf *b, const void *data, size_t len);
int muse_buf_append_str(muse_buf *b, const char *s);
/* Drop the first n bytes, shifting the rest down. */
void muse_buf_consume(muse_buf *b, size_t n);
void muse_buf_clear(muse_buf *b);
void muse_buf_free(muse_buf *b);
/* Detach as a NUL terminated malloc()'d string; the buffer is left empty. */
char *muse_buf_take_str(muse_buf *b);

char *muse_strdup(const char *s);  /* NULL in, NULL out */

/* --- url.c ---------------------------------------------------------------- */
typedef struct muse_url {
    bool secure;        /* https or wss */
    char host[256];     /* without port */
    uint16_t port;
    char path[1024];    /* path plus query, starts with / */
} muse_url;

int muse_url_parse(const char *url, muse_url *out);
/* Percent-encode like JavaScript encodeURIComponent. Returns bytes written
 * excluding the NUL, or MUSE_ETOOBIG. */
int muse_url_encode_component(const char *s, char *out, size_t out_len);

/* --- log.c ---------------------------------------------------------------- */
typedef struct muse_log {
    void *user;
    void (*fn)(void *user, const char *line);
} muse_log;
/* printf-style, formats into a 512 byte line; silent when fn is NULL. */
void muse_logf(const muse_log *log, const char *fmt, ...);

/* --- tls.c: RNG plus the one TLS configuration shared by all connections -- */
typedef struct muse_tls {
    mbedtls_entropy_context entropy;
    mbedtls_ctr_drbg_context drbg;
    mbedtls_x509_crt root;
    mbedtls_ssl_config conf;
    bool ignore_cert_dates;
    muse_log log;
} muse_tls;

/* Seeds the DRBG from muse_port_random (registered as a strong source) on top
 * of mbedTLS's own sources, loads the embedded DigiCert Global Root G2 and
 * configures TLS 1.2 client verification against it alone. */
int muse_tls_init(muse_tls *tls, bool ignore_cert_dates, muse_log log);
void muse_tls_free(muse_tls *tls);
int muse_tls_random(muse_tls *tls, void *buf, size_t len);

/* The root certificate, DER, from musebadge/digicert_global_root_g2.der. */
extern const unsigned char muse_digicert_global_root_g2_der[];
extern const size_t muse_digicert_global_root_g2_der_len;
/* dns.c. muse_dns_query returns the packet's length, or 0 when the name does
 * not fit. muse_dns_answer returns MUSE_OK with the first IPv4 address. */
size_t muse_dns_query(uint8_t *packet, size_t cap, const char *host, uint16_t id);
int muse_dns_answer(const uint8_t *packet, size_t got, uint16_t id, uint8_t address[4]);

extern const unsigned char muse_gts_root_r4_der[];
extern const size_t muse_gts_root_r4_der_len;

/* --- stream.c: a connected TCP or TLS byte stream -------------------------- */
typedef struct muse_stream muse_stream;

/* tls may be NULL only when secure is false. host is used for SNI and
 * hostname verification. */
int muse_stream_open(muse_tls *tls, bool secure, const char *host, uint16_t port,
                     uint32_t timeout_ms, muse_stream **out);
/* >0 bytes read, 0 timeout, MUSE_EIO closed or failed. */
int muse_stream_read(muse_stream *s, void *buf, size_t len, uint32_t timeout_ms);
/* Writes everything or fails: 0, MUSE_ETIMEOUT, MUSE_EIO. */
int muse_stream_write_all(muse_stream *s, const void *buf, size_t len, uint32_t timeout_ms);
void muse_stream_close(muse_stream *s);

/* --- http.c --------------------------------------------------------------- */
typedef struct muse_header {
    const char *name;
    const char *value;
} muse_header;

/* One request on a fresh connection, Connection: close, chunked and
 * Content-Length bodies, body capped at MUSE_MAX_HTTP_BODY. body_out is
 * appended to. Returns 0 with *status set when a response arrived. */
int muse_http_request(muse_tls *tls, bool allow_plaintext, const char *method, const char *url,
                      const muse_header *headers, size_t nheaders, const void *body,
                      size_t body_len, uint32_t timeout_ms, int *status, muse_buf *body_out);

/* --- ws.c: RFC 6455 client, zero mask, ping/pong, fragments --------------- */
typedef struct muse_ws muse_ws;

enum { MUSE_WS_TEXT = 0x1, MUSE_WS_BINARY = 0x2, MUSE_WS_CLOSE = 0x8, MUSE_WS_PING = 0x9, MUSE_WS_PONG = 0xA };

/* Returns MUSE_EAUTH on 401, MUSE_EFORBIDDEN on 403, MUSE_EHTTP on any other
 * non-101 status. */
int muse_ws_connect(muse_tls *tls, bool allow_plaintext, const char *url,
                    const muse_header *headers, size_t nheaders, uint32_t timeout_ms,
                    muse_ws **out);
int muse_ws_send(muse_ws *ws, int opcode, const void *payload, size_t len);
/* One complete message: 1 and *opcode (TEXT or BINARY) with msg replaced,
 * 0 timeout with nothing complete, MUSE_EIO closed, MUSE_EPROTO bad frame.
 * Pings are answered inside; pongs only refresh last_rx. */
int muse_ws_recv(muse_ws *ws, uint32_t timeout_ms, int *opcode, muse_buf *msg);
uint32_t muse_ws_last_rx_ms(const muse_ws *ws);
/* Sends a close frame with a short timeout, then closes the stream. */
void muse_ws_close(muse_ws *ws);

/* --- proto.c: protobuf wire helpers ---------------------------------------- */
enum { MUSE_WIRE_VARINT = 0, MUSE_WIRE_FIXED64 = 1, MUSE_WIRE_DELIMITED = 2, MUSE_WIRE_FIXED32 = 5 };

int muse_pb_put_varint(muse_buf *b, uint64_t v);
int muse_pb_put_key(muse_buf *b, uint32_t field, int wire);
int muse_pb_put_uint(muse_buf *b, uint32_t field, uint64_t v);      /* varint field */
int muse_pb_put_int64(muse_buf *b, uint32_t field, int64_t v);      /* two's complement varint */
int muse_pb_put_bool(muse_buf *b, uint32_t field, bool v);
int muse_pb_put_bytes(muse_buf *b, uint32_t field, const void *data, size_t len);
int muse_pb_put_str(muse_buf *b, uint32_t field, const char *s);

typedef struct muse_pb_reader {
    const uint8_t *data;
    size_t len;
    size_t off;
} muse_pb_reader;

/* Each returns 0, or MUSE_EPROTO on truncation or a malformed varint. */
int muse_pb_read_varint(muse_pb_reader *r, uint64_t *v);
int muse_pb_read_key(muse_pb_reader *r, uint32_t *field, int *wire);
int muse_pb_read_delimited(muse_pb_reader *r, const uint8_t **data, size_t *len);
int muse_pb_skip(muse_pb_reader *r, int wire);

/* --- noise.c: Noise_XX_25519_AESGCM_SHA256 initiator and the service codec */
typedef struct muse_noise muse_noise;

typedef enum muse_frame_kind { MUSE_FRAME_RESPONSE, MUSE_FRAME_BODY_CHUNK, MUSE_FRAME_RESET } muse_frame_kind;

typedef struct muse_frame {
    muse_frame_kind kind;
    int64_t stream_id;
    int status;              /* RESPONSE */
    const uint8_t *data;     /* RESPONSE or BODY_CHUNK, points into the decoded buffer */
    size_t data_len;
    bool end_body;
    char reason[128];        /* RESET */
} muse_frame;

/* Sink for sealed WebSocket binary messages. Returns 0 or an error. */
typedef int (*muse_noise_sink)(void *ctx, const uint8_t *frame, size_t len);

/* Generates both key pairs now (slow on small CPUs, so do it before the
 * socket is open). */
int muse_noise_new(muse_tls *rng, muse_noise **out);
void muse_noise_free(muse_noise *n);
/* Handshake. msg1 is exactly 32 bytes. */
int muse_noise_write_message1(muse_noise *n, uint8_t out[32]);
int muse_noise_read_message2(muse_noise *n, const uint8_t *msg, size_t len);
/* Appends message 3 (48 + 16 bytes) to out. */
int muse_noise_write_message3(muse_noise *n, muse_buf *out);
/* Derives the transport keys and wipes the handshake state. */
int muse_noise_split(muse_noise *n);

/* Transport. Each call seals and emits one or more frames through sink,
 * with consecutive nonces, so callers hold their send lock across a call. */
int muse_noise_send_request(muse_noise *n, const char *verb, const char *path,
                            const muse_header *headers, size_t nheaders, const void *body,
                            size_t body_len, bool end_body, int64_t *stream_id,
                            muse_noise_sink sink, void *ctx);
int muse_noise_send_body_chunk(muse_noise *n, int64_t stream_id, const void *data, size_t len,
                               bool end_body, muse_noise_sink sink, void *ctx);
int muse_noise_send_reset(muse_noise *n, int64_t stream_id, const char *reason,
                          muse_noise_sink sink, void *ctx);
/* Decrypts one inbound frame and feeds the chunk assembler. *complete is set
 * when a whole ServiceResponse is in `out` (replaced). A partial assembly
 * returns 0 with *complete false. */
int muse_noise_receive(muse_noise *n, const uint8_t *ciphertext, size_t len, muse_buf *out,
                       bool *complete);
/* Decodes an assembled ServiceResponse. Returns 0, MUSE_EPROTO, or 1 when the
 * envelope holds nothing this client handles. */
int muse_noise_decode_frame(const uint8_t *data, size_t len, muse_frame *frame);

/* --- api.c: the device API over HTTPS ------------------------------------- */
typedef struct muse_vm {
    char *vm_id;
    char *vm_name;
    char *vm_auth_token;
    bool is_default;
} muse_vm;

/* Fills *vm with the default (else first) VM. Returns 0 with *status 200,
 * MUSE_EAUTH on 401, MUSE_EHTTP on other statuses (status set), MUSE_EIO
 * when no response arrived (status 0), 1 when the list was empty. */
int muse_api_fetch_default_vm(muse_tls *tls, bool allow_plaintext, const char *api_root,
                              const char *access_token, const char *user_agent, muse_log log,
                              int *status, muse_vm *vm);
void muse_vm_free(muse_vm *vm);

/* Rotates the pair. On 0, *access and *refresh are malloc()'d. The refresh
 * token is sent as "hatch_refresh:<raw>" where raw is after the last colon. */
int muse_api_refresh_token(muse_tls *tls, bool allow_plaintext, const char *api_root,
                           const char *refresh_token, const char *device_id,
                           const char *user_agent, const char *sdk_token, muse_log log,
                           int *status, char **access, char **refresh);

/* --- link.c: one session with a VM ---------------------------------------- */
typedef enum muse_outcome {
    MUSE_OUTCOME_CLOSED,        /* reconnect normally */
    MUSE_OUTCOME_AUTH_REJECTED, /* edge refused the VM bearer */
    MUSE_OUTCOME_FORBIDDEN,
    MUSE_OUTCOME_UNPAIRED,
    MUSE_OUTCOME_STOPPED
} muse_outcome;

/* The one pending chat turn handed from any thread to the serve loop. The
 * owner (muse.c) guards it with the mutex; link.c reads it under the same
 * lock and takes ownership of the bytes. */
typedef enum muse_ask_kind { MUSE_ASK_NONE = 0, MUSE_ASK_TEXT, MUSE_ASK_VOICE } muse_ask_kind;

typedef struct muse_ask {
    muse_ask_kind kind;
    uint8_t *data;     /* UTF-8 text or WAV bytes, malloc()'d; text is NUL terminated */
    size_t len;        /* excludes the text's NUL */
} muse_ask;

typedef struct muse_link_params {
    muse_tls *tls;
    bool allow_plaintext;
    const char *noise_url;       /* ws[s]://host[:port]/v1/noise?vm_id=... */
    const char *vm_auth_token;
    const char *register_json;   /* params object for link.register */
    const char *node_id;         /* device_id in chat bodies */
    const muse_callbacks *cb;
    muse_log log;
    /* Shared with other threads under lock. */
    muse_mutex *lock;
    volatile int *stop_flag;
    muse_ask *ask;               /* set by muse_ask_*; consumed by the session */
    volatile int *ask_pending;   /* 1 while an ask is queued or in flight */
    uint32_t *registered_at_ms;  /* 0 until link.register is acknowledged */
} muse_link_params;

/* Connects, handshakes, registers and serves until the connection ends or
 * *stop_flag is set. Reports CONNECTED through cb->on_state once registered. */
muse_outcome muse_link_run(const muse_link_params *p);

#endif
