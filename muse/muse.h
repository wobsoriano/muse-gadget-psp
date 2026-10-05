/* Muse gadget client: keeps one device registered with its Muse and relays
 * commands and chat turns. Portable C99; platform hooks are in muse_port.h. */
#ifndef MUSE_H
#define MUSE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum muse_state {
    MUSE_STATE_UNPAIRED = 0,
    MUSE_STATE_CONNECTING = 1,
    MUSE_STATE_CONNECTED = 2
} muse_state;

typedef enum muse_run_end {
    MUSE_RUN_STOPPED = 0,   /* muse_stop() was called */
    MUSE_RUN_UNPAIRED = 1   /* the Muse removed this device or revoked its tokens */
} muse_run_end;

/* Return codes shared by every muse_* call that returns int. 0 is success. */
enum {
    MUSE_OK = 0,
    MUSE_EINVAL = -1,
    MUSE_ENOMEM = -2,
    MUSE_EBUSY = -3,       /* a chat turn is already in flight */
    MUSE_EOFFLINE = -4,    /* not connected to a VM */
    MUSE_ETOOBIG = -5,     /* exceeds a size cap */
    MUSE_EIO = -6,         /* transport failure or peer closed */
    MUSE_ETIMEOUT = -7,
    MUSE_EPROTO = -8,      /* malformed data from the peer */
    MUSE_ETLS = -9,        /* TLS handshake or certificate verification failed */
    MUSE_EAUTH = -10,      /* HTTP 401 */
    MUSE_EFORBIDDEN = -11, /* HTTP 403 */
    MUSE_EHTTP = -12       /* other unexpected HTTP status */
};

typedef struct muse_callbacks {
    void *user;
    /* All callbacks run on the thread that called muse_run(). */
    void (*on_state)(void *user, muse_state state);
    /* Tokens rotated; persist both now. The previous pair is dead. */
    void (*on_tokens)(void *user, const char *access_token, const char *refresh_token,
                      int64_t saved_at_unix);
    /* Handle link.invoke. Return malloc()'d JSON text such as
     * {"ok":true,"payload":{...}} or {"ok":false,"error":"..."}; the library
     * free()s it. Returning NULL reports {"ok":false,"error":"unhandled"}. */
    char *(*on_invoke)(void *user, const char *command, const char *params_json);
    /* Reply to muse_ask_text / muse_ask_voice. Called with the text so far
     * while it streams, then once with final=true. When error is non-NULL the
     * turn failed; text may still hold whatever arrived. */
    void (*on_reply)(void *user, const char *text, bool final, const char *error);
    void (*on_log)(void *user, const char *line);
    /* Every reply message so far is complete and the stream has paused. This
     * is almost always the whole answer, a couple of seconds before the
     * final on_reply confirms it, so it is the moment to start anything slow.
     * A later message can still extend the text. Called once per text. */
    void (*on_settled)(void *user, const char *text);
} muse_callbacks;

typedef struct muse_config {
    const char *api_root;        /* NULL: https://api.muse.ai */
    const char *noise_host;      /* "host[:port]" or "ws[s]://host[:port]"; NULL: hatch.metaaivm.com */
    const char *access_token;
    const char *refresh_token;   /* with or without the hatch_refresh: prefix */
    int64_t access_token_saved_at; /* unix seconds; 0 forces a refresh before first use */
    const char *node_id;
    const char *display_name;
    const char *sdk_token;       /* may be NULL */
    const char *commands_json;   /* JSON object for commands_v2; NULL: {} */
    const char *version;
    const char *user_agent;
    const char *network_ssid;    /* may be NULL */
    bool allow_plaintext;        /* permit http:// and ws:// targets (tests only) */
    /* Accept certificates whose validity window excludes the current clock.
     * For a device whose clock is not set. Every other check still applies. */
    bool ignore_cert_dates;
    muse_callbacks callbacks;
} muse_config;

typedef struct muse muse;

/* Copies every string out of config. Returns NULL on bad config or no memory. */
muse *muse_new(const muse_config *config);
/* Only after muse_run() has returned. */
void muse_free(muse *m);

/* Blocking. Refreshes tokens when due, fetches VMs, connects, serves, backs
 * off and reconnects until muse_stop() or the device is unpaired. */
muse_run_end muse_run(muse *m);
/* Any thread. muse_run() returns within a few seconds. */
void muse_stop(muse *m);

/* Any thread. Queue one chat turn; the reply arrives through on_reply.
 * One turn at a time: MUSE_EBUSY while another is pending. The bytes are
 * copied. A turn queued while offline fails through on_reply once the loop
 * notices, so the caller sees exactly one final callback per accepted call. */
int muse_ask_text(muse *m, const char *text);
int muse_ask_voice(muse *m, const void *wav, size_t wav_len);

muse_state muse_get_state(const muse *m);

/* HTTPS to services beside the Muse (speech), in two steps so the slow part
 * can be done ahead of time: muse_https_open connects and completes the TLS
 * handshake, and muse_https_post sends one request on that connection and
 * closes it. The certificate is verified against the embedded roots,
 * DigiCert Global Root G2 and GTS Root R4.
 *
 * The response body is handed to on_data as it arrives, in pieces of any
 * size, together with the HTTP status; a nonzero return stops the transfer
 * with MUSE_ETOOBIG. muse_https_post returns MUSE_OK when a complete
 * response arrived, whatever its status, and MUSE_EIO with *status 0 when
 * the connection had gone stale while it waited, which is worth one retry
 * on a fresh one. It always consumes the connection. */
typedef struct muse_https muse_https;
muse_https *muse_https_open(const char *url, uint32_t timeout_ms,
                            void (*log)(void *user, const char *line), void *log_user);
void muse_https_close(muse_https *connection);
int muse_https_post(muse_https *connection, const char *url, const char *bearer,
                    const char *content_type, const void *body, size_t body_len,
                    uint32_t timeout_ms,
                    int (*on_data)(void *user, int status, const void *data, size_t len),
                    void *data_user, int *status);

/* One HTTPS GET with certificate verification against the embedded DigiCert
 * Global Root G2, for diagnostics. *body is malloc()'d, NUL terminated, and
 * capped at 256 KB; the caller free()s it. Returns MUSE_OK when an HTTP
 * response arrived (whatever its status), MUSE_ETLS when verification failed. */
int muse_https_get(const char *url, const char *user_agent, bool ignore_cert_dates,
                   int *status, char **body);

#ifdef __cplusplus
}
#endif

#endif
