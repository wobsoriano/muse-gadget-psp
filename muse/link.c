/* One control session with a Muse VM. Ported from the Muse Gadget SDK's
 * link_client.py by way of musebadge/link.py, onto one thread: the serve
 * loop reads in short slices and runs the keepalive and the chat turn in
 * between, where the Python used tasks. */
#include "muse_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <cjson/cJSON.h>
#include <mbedtls/base64.h>

#define CONTROL_PATH "/link-control"
#define CHAT_PATH "/chat/stream"
#define SUBSCRIBE_PATH "/chat/subscribe"
#define APP_ID "musegadget"
#define MAX_PENDING_EVENTS 256
#define MAX_REPLY_MESSAGES 32u
#define UUID_LEN 37u

/* --- the turn: the events of one question and the reply they add up to ------ */

typedef enum event_kind {
    EVENT_IGNORED,
    EVENT_STATUS,
    EVENT_USER,
    EVENT_START,
    EVENT_APPEND,
    EVENT_DONE,
    EVENT_ASSISTANT
} event_kind;

static const struct {
    const char *name;
    event_kind kind;
} EVENT_KINDS[] = {
    {"agent.status", EVENT_STATUS},
    {"task.status", EVENT_STATUS},
    {"message.user", EVENT_USER},
    {"delta.message_start", EVENT_START},
    {"delta.text_append", EVENT_APPEND},
    {"delta.message_done", EVENT_DONE},
    {"message.assistant", EVENT_ASSISTANT},
};

typedef struct reply_msg {
    char *id;
    muse_buf text;
    bool done;
} reply_msg;

typedef struct turn {
    bool busy;
    uint32_t last_event_ms;
    char *message_id;      /* NULL until the ack names our message */
    bool ours_seen;        /* the stream has echoed our message */
    /* Reply events can arrive before the ack says what our message id is,
     * so they wait here until it does. NULL once the id is known. */
    cJSON *pending;
    int npending;
    bool overflow_logged;
    reply_msg msgs[MAX_REPLY_MESSAGES];
    size_t nmsgs;
    bool dirty;            /* the reply changed since it was last shown */
} turn;

static const char *string_item(const cJSON *object, const char *key)
{
    const cJSON *item = cJSON_GetObjectItemCaseSensitive(object, key);
    return cJSON_IsString(item) && item->valuestring[0] != '\0' ? item->valuestring : NULL;
}

static bool truthy(const cJSON *item)
{
    if (item == NULL || cJSON_IsNull(item) || cJSON_IsFalse(item)) {
        return false;
    }
    if (cJSON_IsString(item)) {
        return item->valuestring[0] != '\0';
    }
    if (cJSON_IsNumber(item)) {
        return item->valuedouble != 0;
    }
    if (cJSON_IsArray(item) || cJSON_IsObject(item)) {
        return item->child != NULL;
    }
    return true;
}

static event_kind classify(const cJSON *event)
{
    const char *name = string_item(event, "event");
    for (size_t i = 0; name != NULL && i < sizeof EVENT_KINDS / sizeof EVENT_KINDS[0]; i++) {
        if (strcmp(name, EVENT_KINDS[i].name) == 0) {
            return EVENT_KINDS[i].kind;
        }
    }
    return EVENT_IGNORED;
}

static const cJSON *event_payload(const cJSON *event)
{
    const cJSON *payload = cJSON_GetObjectItemCaseSensitive(event, "payload");
    return cJSON_IsObject(payload) ? payload : NULL;
}

/* Appends what fits, cut on a UTF-8 character boundary. */
static void append_capped(muse_buf *b, const char *s)
{
    size_t len = strlen(s);
    size_t room = b->max - b->len;
    if (len > room) {
        len = room;
        while (len > 0 && ((unsigned char)s[len] & 0xC0u) == 0x80u) {
            len--;
        }
    }
    (void)muse_buf_append(b, s, len);
}

static void turn_init(turn *t)
{
    memset(t, 0, sizeof *t);
    t->last_event_ms = muse_port_now_ms();
    t->pending = cJSON_CreateArray();
}

static void turn_free(turn *t)
{
    for (size_t i = 0; i < t->nmsgs; i++) {
        free(t->msgs[i].id);
        muse_buf_free(&t->msgs[i].text);
    }
    free(t->message_id);
    cJSON_Delete(t->pending);
    memset(t, 0, sizeof *t);
}

static reply_msg *turn_find(turn *t, const char *id)
{
    for (size_t i = 0; i < t->nmsgs; i++) {
        if (strcmp(t->msgs[i].id, id) == 0) {
            return &t->msgs[i];
        }
    }
    return NULL;
}

/* One step of _Turn.reply() in link.py. The Muse does not link its reply to
 * the question, so a message that starts after ours belongs to the turn:
 * after the stream echoes our message, or (follows_allowed) after the ack. */
static void turn_apply(turn *t, const cJSON *event, bool follows_allowed)
{
    event_kind kind = classify(event);
    const cJSON *payload = event_payload(event);
    const char *id = string_item(payload, "message_id");
    if (id == NULL) {
        id = string_item(event, "message_id");
    }
    if (id == NULL) {
        id = string_item(payload, "id");
    }
    if (id == NULL) {
        id = "";
    }

    if (kind == EVENT_USER) {
        t->ours_seen = t->ours_seen || strcmp(id, t->message_id) == 0;
        return;
    }

    reply_msg *m = turn_find(t, id);
    if (m == NULL) {
        if (kind == EVENT_APPEND) {
            return;
        }
        const char *parent = string_item(payload, "reply_to_message_id");
        if (parent == NULL) {
            parent = string_item(payload, "parent_message_id");
        }
        bool linked = parent != NULL &&
                      (strcmp(parent, t->message_id) == 0 || turn_find(t, parent) != NULL);
        bool follows = parent == NULL && (t->ours_seen || follows_allowed);
        if (!(linked || follows) || t->nmsgs == MAX_REPLY_MESSAGES) {
            return;
        }
        char *copy = muse_strdup(id);
        if (copy == NULL) {
            return;
        }
        m = &t->msgs[t->nmsgs++];
        m->id = copy;
        m->done = false;
        muse_buf_init(&m->text, MUSE_MAX_REPLY_TEXT);
        t->dirty = true;
    }

    if (kind == EVENT_APPEND) {
        const char *text = string_item(payload, "text");
        if (text != NULL) {
            append_capped(&m->text, text);
            t->dirty = true;
        }
        return;
    }
    if (kind == EVENT_START) {
        return;
    }
    const cJSON *final = cJSON_GetObjectItemCaseSensitive(payload, "display_text");
    if (!truthy(final)) {
        final = cJSON_GetObjectItemCaseSensitive(payload, "content");
    }
    if (cJSON_IsString(final) && final->valuestring[0] != '\0') {
        muse_buf_clear(&m->text);
        append_capped(&m->text, final->valuestring);
        t->dirty = true;
    }
    if (kind == EVENT_DONE ||
        !cJSON_IsFalse(cJSON_GetObjectItemCaseSensitive(payload, "display_text_ready"))) {
        m->done = true;
        t->dirty = true;
    }
}

/* Takes ownership of event. */
static void turn_add(turn *t, cJSON *event, const muse_log *log)
{
    t->last_event_ms = muse_port_now_ms();
    event_kind kind = classify(event);
    if (kind == EVENT_STATUS) {
        const cJSON *payload = event_payload(event);
        const cJSON *activity = cJSON_GetObjectItemCaseSensitive(payload, "activity_code");
        const cJSON *status = cJSON_GetObjectItemCaseSensitive(payload, "status");
        if (truthy(activity)) {
            const char *code = cJSON_IsString(activity) ? activity->valuestring : "";
            t->busy = strcmp(code, "online") != 0 && strcmp(code, "idle") != 0;
        } else if (truthy(status)) {
            const char *state = cJSON_IsString(status) ? status->valuestring : "";
            t->busy = strcmp(state, "completed") != 0 && strcmp(state, "failed") != 0;
        }
    } else if (kind != EVENT_IGNORED) {
        if (t->message_id != NULL) {
            turn_apply(t, event, true);
        } else if (t->pending != NULL && t->npending < MAX_PENDING_EVENTS) {
            cJSON_AddItemToArray(t->pending, event);
            t->npending++;
            return;
        } else if (!t->overflow_logged) {
            t->overflow_logged = true;
            muse_logf(log, "too many chat events before the Muse acknowledged; dropping some");
        }
    }
    cJSON_Delete(event);
}

static void turn_set_message_id(turn *t, const char *message_id)
{
    t->message_id = muse_strdup(message_id);
    if (t->message_id == NULL) {
        return;
    }
    t->ours_seen = false;
    const cJSON *event;
    cJSON_ArrayForEach(event, t->pending) {
        turn_apply(t, event, false);
    }
    cJSON_Delete(t->pending);
    t->pending = NULL;
    t->npending = 0;
}

static bool turn_done(const turn *t)
{
    for (size_t i = 0; i < t->nmsgs; i++) {
        if (!t->msgs[i].done) {
            return false;
        }
    }
    return t->nmsgs > 0;
}

static char *turn_text(const turn *t)
{
    muse_buf out;
    muse_buf_init(&out, MUSE_MAX_REPLY_TEXT);
    for (size_t i = 0; i < t->nmsgs; i++) {
        const muse_buf *text = &t->msgs[i].text;
        if (text->len == 0) {
            continue;
        }
        size_t separator = out.len > 0 ? 2u : 0u;
        if (out.len + separator + text->len > out.max) {
            break;
        }
        (void)muse_buf_append(&out, "\n\n", separator);
        (void)muse_buf_append(&out, text->data, text->len);
    }
    return muse_buf_take_str(&out);
}

/* --- session ---------------------------------------------------------------- */

typedef struct subscription {
    bool open;
    bool closed;
    int64_t id;
    muse_buf line;         /* the NDJSON line being assembled */
    char error[160];
} subscription;

/* The response to one request stream (the chat ack). */
typedef struct request {
    bool active;
    bool done;
    int64_t id;
    int status;
    muse_buf body;
    uint32_t sent_ms;
    char error[160];
} request;

typedef enum chat_phase {
    CHAT_IDLE,        /* no ask accepted */
    CHAT_AWAIT_ACK,   /* message sent, waiting for the Muse to take it */
    CHAT_REPLYING     /* acked, the reply is streaming on the subscription */
} chat_phase;

typedef struct chat {
    chat_phase phase;
    uint32_t started_ms;
    request ack;
    turn turn;
    char *shown;           /* reply text last delivered through on_reply */
    bool settled;          /* on_settled has been called for shown */
} chat;

typedef struct keepalive {
    uint32_t cycle_start_ms;
    bool ping_pending;
    uint32_t ping_sent_ms;
} keepalive;

typedef struct session {
    const muse_link_params *p;
    muse_ws *ws;
    muse_noise *noise;
    bool ended;
    muse_outcome outcome;
    bool registered;
    int64_t control_id;
    char register_id[UUID_LEN];
    muse_buf control;      /* length-prefixed JSON messages being assembled */
    muse_buf rx;
    muse_buf assembled;
    subscription sub;
    chat chat;
    keepalive keepalive;
} session;

static void session_end(session *s, muse_outcome outcome)
{
    if (!s->ended) {
        s->ended = true;
        s->outcome = outcome;
    }
}

typedef enum out_kind { OUT_RAW, OUT_PING, OUT_REQUEST, OUT_BODY } out_kind;

typedef struct outbound {
    out_kind kind;
    const char *path;              /* REQUEST */
    const muse_header *headers;    /* REQUEST */
    size_t nheaders;
    const void *data;
    size_t len;
    bool end_body;
    int64_t stream_id;             /* BODY: target. REQUEST: receives the new id. */
} outbound;

static int ws_sink(void *ctx, const uint8_t *frame, size_t len)
{
    return muse_ws_send(((session *)ctx)->ws, MUSE_WS_BINARY, frame, len);
}

/* Every outbound byte goes through here. Frames carry consecutive nonces, so
 * sealing and writing one message must not interleave with another. */
static int session_send(session *s, outbound *o)
{
    int rc = MUSE_EINVAL;
    muse_port_mutex_lock(s->p->lock);
    switch (o->kind) {
    case OUT_RAW:
        rc = muse_ws_send(s->ws, MUSE_WS_BINARY, o->data, o->len);
        break;
    case OUT_PING:
        rc = muse_ws_send(s->ws, MUSE_WS_PING, NULL, 0);
        break;
    case OUT_REQUEST:
        rc = muse_noise_send_request(s->noise, "POST", o->path, o->headers, o->nheaders, o->data,
                                     o->len, o->end_body, &o->stream_id, ws_sink, s);
        break;
    case OUT_BODY:
        rc = muse_noise_send_body_chunk(s->noise, o->stream_id, o->data, o->len, o->end_body,
                                        ws_sink, s);
        break;
    }
    muse_port_mutex_unlock(s->p->lock);
    if (rc != 0) {
        muse_logf(&s->p->log, "write to the Muse failed: error %d", rc);
        session_end(s, MUSE_OUTCOME_CLOSED);
    }
    return rc;
}

static int uuid4(session *s, char out[UUID_LEN])
{
    uint8_t b[16];
    if (muse_tls_random(s->p->tls, b, sizeof b) != 0) {
        return MUSE_EIO;
    }
    b[6] = (uint8_t)((b[6] & 0x0Fu) | 0x40u);
    b[8] = (uint8_t)((b[8] & 0x3Fu) | 0x80u);
    snprintf(out, UUID_LEN, "%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%02x%02x%02x%02x%02x%02x",
             b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
             b[14], b[15]);
    return 0;
}

/* encode_message: compact JSON behind its length as a little-endian u32. */
static int send_control_message(session *s, const cJSON *message)
{
    char *text = cJSON_PrintUnformatted(message);
    if (text == NULL) {
        return MUSE_ENOMEM;
    }
    size_t len = strlen(text);
    int rc = MUSE_ENOMEM;
    uint8_t *framed = len <= MUSE_MAX_INBOUND_MESSAGE ? malloc(len + 4) : NULL;
    if (framed != NULL) {
        for (unsigned i = 0; i < 4; i++) {
            framed[i] = (uint8_t)(len >> (8u * i));
        }
        memcpy(framed + 4, text, len);
        outbound o = {.kind = OUT_BODY, .stream_id = s->control_id, .data = framed, .len = len + 4};
        rc = session_send(s, &o);
        free(framed);
    }
    cJSON_free(text);
    return rc;
}

/* --- chat --------------------------------------------------------------------- */

static void chat_reset(chat *c)
{
    turn_free(&c->turn);
    muse_buf_free(&c->ack.body);
    free(c->shown);
    memset(c, 0, sizeof *c);
}

/* The one final on_reply of an accepted ask. error NULL means the reply is
 * complete. */
static void chat_finish(session *s, const char *error)
{
    const muse_callbacks *cb = s->p->cb;
    if (cb->on_reply != NULL) {
        cb->on_reply(cb->user, s->chat.shown != NULL ? s->chat.shown : "", true, error);
    }
    chat_reset(&s->chat);
    muse_port_mutex_lock(s->p->lock);
    *s->p->ask_pending = 0;
    muse_port_mutex_unlock(s->p->lock);
}

static int open_subscription(session *s)
{
    char request_id[UUID_LEN];
    if (uuid4(s, request_id) != 0) {
        return MUSE_EIO;
    }
    const muse_header headers[] = {
        {"Content-Type", "application/json"},
        {"accept", "application/x-ndjson"},
        {"x-request-id", request_id},
        {"x-app-id", APP_ID},
    };
    outbound o = {.kind = OUT_REQUEST, .path = SUBSCRIBE_PATH, .headers = headers,
                  .nheaders = sizeof headers / sizeof headers[0], .data = "{}", .len = 2,
                  .end_body = true};
    int rc = session_send(s, &o);
    if (rc != 0) {
        return rc;
    }
    muse_buf_clear(&s->sub.line);
    s->sub.open = true;
    s->sub.closed = false;
    s->sub.error[0] = '\0';
    s->sub.id = o.stream_id;
    return 0;
}

static char *text_chat_body(session *s, const muse_ask *ask)
{
    cJSON *body = cJSON_CreateObject();
    char *text = NULL;
    if (body != NULL && cJSON_AddStringToObject(body, "message", (const char *)ask->data) != NULL &&
        cJSON_AddStringToObject(body, "output_modality", "text") != NULL &&
        cJSON_AddStringToObject(body, "device_id", s->p->node_id) != NULL) {
        text = cJSON_PrintUnformatted(body);
    }
    cJSON_Delete(body);
    return text;
}

/* The WAV rides as a base64 attachment, the way Meta's ESP32 voice gadgets
 * send it. Built by hand so the audio is encoded once, straight into place. */
static int voice_chat_body(const muse_ask *ask, muse_buf *body)
{
    static const char prefix[] =
        "{\"message\":\"\",\"output_modality\":\"text\",\"items\":[{\"type\":\"file\","
        "\"mime_type\":\"audio/wav\",\"filename\":\"voice_note.wav\",\"data_base64\":\"";
    static const char suffix[] = "\"}]}";
    muse_buf_init(body, 0);
    if (ask->len > MUSE_MAX_VOICE_WAV) {
        return MUSE_ETOOBIG;
    }
    size_t encoded = 4u * ((ask->len + 2u) / 3u);
    size_t total = sizeof prefix - 1 + encoded + 1 + sizeof suffix - 1;
    body->max = total;
    int rc = muse_buf_reserve(body, total);
    if (rc == 0) {
        rc = muse_buf_append(body, prefix, sizeof prefix - 1);
    }
    if (rc == 0) {
        size_t written = 0;
        if (mbedtls_base64_encode(body->data + body->len, encoded + 1, &written, ask->data,
                                  ask->len) != 0) {
            rc = MUSE_EINVAL;
        }
        body->len += written;
    }
    if (rc == 0) {
        rc = muse_buf_append(body, suffix, sizeof suffix - 1);
    }
    return rc;
}

static int send_chat(session *s, const muse_ask *ask)
{
    char request_id[UUID_LEN];
    if (uuid4(s, request_id) != 0) {
        return MUSE_EIO;
    }
    const muse_header headers[] = {
        {"x-request-id", request_id},
        {"x-app-id", APP_ID},
        {"Content-Type", "application/json"},
    };
    outbound open = {.kind = OUT_REQUEST, .path = CHAT_PATH, .headers = headers,
                     .nheaders = sizeof headers / sizeof headers[0]};
    request *ack = &s->chat.ack;
    int rc;

    if (ask->kind == MUSE_ASK_TEXT) {
        char *body = text_chat_body(s, ask);
        if (body == NULL) {
            return MUSE_ENOMEM;
        }
        open.data = body;
        open.len = strlen(body);
        open.end_body = true;
        rc = session_send(s, &open);
        cJSON_free(body);
    } else {
        muse_buf body;
        rc = voice_chat_body(ask, &body);
        if (rc == 0) {
            rc = session_send(s, &open);
        }
        for (size_t start = 0; rc == 0 && start < body.len; start += MUSE_BODY_CHUNK) {
            size_t left = body.len - start;
            outbound piece = {.kind = OUT_BODY, .stream_id = open.stream_id,
                              .data = body.data + start,
                              .len = left < MUSE_BODY_CHUNK ? left : MUSE_BODY_CHUNK,
                              .end_body = left <= MUSE_BODY_CHUNK};
            rc = session_send(s, &piece);
        }
        muse_buf_free(&body);
    }
    if (rc != 0) {
        return rc;
    }
    muse_buf_init(&ack->body, MUSE_MAX_RESPONSE_BYTES);
    ack->active = true;
    ack->done = false;
    ack->status = 0;
    ack->error[0] = '\0';
    ack->id = open.stream_id;
    ack->sent_ms = muse_port_now_ms();
    return 0;
}

static void chat_start(session *s, muse_ask *ask)
{
    turn_init(&s->chat.turn);
    s->chat.phase = CHAT_AWAIT_ACK;
    int rc = 0;
    if (!s->sub.open || s->sub.closed) {
        rc = open_subscription(s);
    }
    if (rc == 0) {
        rc = send_chat(s, ask);
    }
    free(ask->data);
    ask->data = NULL;
    if (rc != 0 && !s->ended) {
        chat_finish(s, rc == MUSE_ETOOBIG ? "the message is too large" : "could not send the message");
    }
}

static void chat_take_ask(session *s)
{
    muse_ask ask = {MUSE_ASK_NONE, NULL, 0};
    muse_port_mutex_lock(s->p->lock);
    if (*s->p->ask_pending && s->p->ask->kind != MUSE_ASK_NONE) {
        ask = *s->p->ask;
        s->p->ask->kind = MUSE_ASK_NONE;
        s->p->ask->data = NULL;
        s->p->ask->len = 0;
    }
    muse_port_mutex_unlock(s->p->lock);
    if (ask.kind != MUSE_ASK_NONE) {
        chat_start(s, &ask);
    }
}

static void chat_on_ack(session *s)
{
    request *ack = &s->chat.ack;
    if (ack->error[0] != '\0') {
        chat_finish(s, ack->error);
        return;
    }
    if (ack->status < 200 || ack->status >= 300) {
        char error[64];
        snprintf(error, sizeof error, "the Muse did not take the message: HTTP %d", ack->status);
        chat_finish(s, error);
        return;
    }
    cJSON *response = cJSON_ParseWithLength((const char *)ack->body.data, ack->body.len);
    const cJSON *result = cJSON_GetObjectItemCaseSensitive(response, "result");
    const char *message_id = string_item(cJSON_IsObject(result) ? result : response, "message_id");
    if (message_id != NULL) {
        turn_set_message_id(&s->chat.turn, message_id);
    }
    cJSON_Delete(response);
    muse_buf_free(&ack->body);
    ack->active = false;
    s->chat.started_ms = muse_port_now_ms();
    s->chat.phase = CHAT_REPLYING;
}

static void chat_show_progress(session *s)
{
    turn *t = &s->chat.turn;
    if (!t->dirty) {
        return;
    }
    t->dirty = false;
    char *text = turn_text(t);
    if (text == NULL) {
        return;
    }
    if (strcmp(text, s->chat.shown != NULL ? s->chat.shown : "") == 0) {
        free(text);
        return;
    }
    free(s->chat.shown);
    s->chat.shown = text;
    s->chat.settled = false;
    const muse_callbacks *cb = s->p->cb;
    if (cb->on_reply != NULL) {
        cb->on_reply(cb->user, text, false, NULL);
    }
}

/* Nothing marks the end of a turn, so it is over once every reply message is
 * done and the stream has been quiet for a moment. */
static void chat_step(session *s)
{
    chat *c = &s->chat;
    uint32_t now = muse_port_now_ms();
    switch (c->phase) {
    case CHAT_IDLE:
        if (s->registered) {
            chat_take_ask(s);
        }
        return;
    case CHAT_AWAIT_ACK:
        if (c->ack.done) {
            chat_on_ack(s);
        } else if ((uint32_t)(now - c->ack.sent_ms) > MUSE_REQUEST_TIMEOUT_MS) {
            chat_finish(s, "the Muse did not acknowledge the message");
        }
        return;
    case CHAT_REPLYING:
        break;
    }

    chat_show_progress(s);
    bool has_text = c->shown != NULL && c->shown[0] != '\0';
    bool quiet = (uint32_t)(now - c->turn.last_event_ms) > MUSE_REPLY_QUIET_MS;
    uint32_t elapsed = (uint32_t)(now - c->started_ms);
    bool whole = has_text && turn_done(&c->turn) && !c->turn.busy;
    if (whole && !c->settled
        && (uint32_t)(now - c->turn.last_event_ms) > MUSE_REPLY_SETTLED_MS) {
        c->settled = true;
        if (s->p->cb->on_settled != NULL) {
            s->p->cb->on_settled(s->p->cb->user, c->shown);
        }
    }
    if (whole && quiet) {
        chat_finish(s, NULL);
    } else if (s->sub.closed) {
        char error[200];
        snprintf(error, sizeof error, "chat stream closed: %s", s->sub.error);
        chat_finish(s, error);
    } else if (!has_text && elapsed > MUSE_FIRST_REPLY_MS) {
        chat_finish(s, "the Muse did not reply");
    } else if (elapsed > MUSE_TURN_CAP_MS) {
        chat_finish(s, has_text ? NULL : "the Muse did not finish replying");
    }
}

/* --- inbound streams ---------------------------------------------------------- */

static void on_request_frame(session *s, const muse_frame *f)
{
    request *r = &s->chat.ack;
    if (r->done) {
        return;
    }
    if (f->kind == MUSE_FRAME_RESET) {
        snprintf(r->error, sizeof r->error, "stream reset: %s", f->reason);
        r->done = true;
        return;
    }
    if (f->kind == MUSE_FRAME_RESPONSE) {
        r->status = f->status;
    }
    if (muse_buf_append(&r->body, f->data, f->data_len) != 0) {
        snprintf(r->error, sizeof r->error, "response too large");
        r->done = true;
    } else if (f->end_body) {
        r->done = true;
    }
}

static void on_event_line(session *s, const uint8_t *line, size_t len)
{
    cJSON *event = cJSON_ParseWithLength((const char *)line, len);
    const char *type = string_item(event, "type");
    /* Other line types are the subscribe ack. */
    if (cJSON_IsObject(event) && type != NULL && strcmp(type, "event") == 0 &&
        s->chat.phase != CHAT_IDLE) {
        turn_add(&s->chat.turn, event, &s->p->log);
    } else {
        cJSON_Delete(event);
    }
}

static void subscription_close(subscription *sub, const char *error)
{
    sub->closed = true;
    snprintf(sub->error, sizeof sub->error, "%s", error);
}

static void on_subscription_frame(session *s, const muse_frame *f)
{
    subscription *sub = &s->sub;
    if (sub->closed) {
        return;
    }
    if (f->kind == MUSE_FRAME_RESET) {
        subscription_close(sub, f->reason);
        return;
    }
    if (f->kind == MUSE_FRAME_RESPONSE && f->status >= 400) {
        char error[32];
        snprintf(error, sizeof error, "HTTP %d", f->status);
        subscription_close(sub, error);
        return;
    }
    const uint8_t *at = f->data;
    const uint8_t *end = f->data + f->data_len;
    while (at < end) {
        const uint8_t *newline = memchr(at, '\n', (size_t)(end - at));
        const uint8_t *stop = newline != NULL ? newline : end;
        if (muse_buf_append(&sub->line, at, (size_t)(stop - at)) != 0) {
            subscription_close(sub, "event line too long");
            return;
        }
        if (newline == NULL) {
            break;
        }
        on_event_line(s, sub->line.data, sub->line.len);
        muse_buf_clear(&sub->line);
        at = newline + 1;
    }
    if (f->end_body) {
        subscription_close(sub, "ended by the Muse");
    }
}

static cJSON *error_result(const char *message)
{
    cJSON *result = cJSON_CreateObject();
    if (result != NULL) {
        cJSON_AddFalseToObject(result, "ok");
        cJSON_AddStringToObject(result, "error", message);
    }
    return result;
}

static cJSON *run_command(session *s, const char *command, const cJSON *params)
{
    const muse_callbacks *cb = s->p->cb;
    char *params_text = cJSON_IsObject(params) ? cJSON_PrintUnformatted(params) : NULL;
    char *result_text = NULL;
    if (cb->on_invoke != NULL) {
        result_text = cb->on_invoke(cb->user, command, params_text != NULL ? params_text : "{}");
    }
    cJSON_free(params_text);
    if (result_text == NULL) {
        return error_result("unhandled");
    }
    cJSON *result = cJSON_Parse(result_text);
    free(result_text);
    if (!cJSON_IsObject(result)) {
        cJSON_Delete(result);
        return error_result("invalid result");
    }
    return result;
}

static void on_invoke(session *s, const cJSON *message)
{
    const cJSON *id = cJSON_GetObjectItemCaseSensitive(message, "id");
    if (!truthy(id)) {
        return;
    }
    const char *command = string_item(message, "command");
    if (command == NULL) {
        command = "";
    }
    muse_logf(&s->p->log, "invoke %s", command);
    cJSON *result = run_command(s, command, cJSON_GetObjectItemCaseSensitive(message, "params"));
    cJSON *reply = cJSON_CreateObject();
    cJSON *id_copy = cJSON_Duplicate(id, 1);
    if (result == NULL || reply == NULL || id_copy == NULL ||
        cJSON_AddStringToObject(reply, "method", "link.result") == NULL) {
        cJSON_Delete(id_copy);
        cJSON_Delete(result);
        cJSON_Delete(reply);
        return;
    }
    cJSON_AddItemToObject(reply, "id", id_copy);
    /* The result's fields win, as dict.update does in the Python. */
    while (result->child != NULL) {
        cJSON *field = cJSON_DetachItemViaPointer(result, result->child);
        cJSON_DeleteItemFromObjectCaseSensitive(reply, field->string);
        cJSON_AddItemToObject(reply, field->string, field);
    }
    cJSON_Delete(result);
    (void)send_control_message(s, reply);
    cJSON_Delete(reply);
}

static void on_control_message(session *s, const cJSON *message)
{
    const char *id = string_item(message, "id");
    const cJSON *method = cJSON_GetObjectItemCaseSensitive(message, "method");
    if (id != NULL && strcmp(id, s->register_id) == 0 && (method == NULL || cJSON_IsNull(method))) {
        const cJSON *error = cJSON_GetObjectItemCaseSensitive(message, "error");
        if (truthy(error)) {
            muse_logf(&s->p->log, "link.register rejected: %s",
                      cJSON_IsString(error) ? error->valuestring : "error");
        } else if (!s->registered) {
            uint32_t now = muse_port_now_ms();
            s->registered = true;
            *s->p->registered_at_ms = now != 0 ? now : 1;
            muse_logf(&s->p->log, "registered with the Muse");
            if (s->p->cb->on_state != NULL) {
                s->p->cb->on_state(s->p->cb->user, MUSE_STATE_CONNECTED);
            }
        }
        return;
    }
    const char *event = string_item(message, "event");
    if (event != NULL && (strcmp(event, "link.unpaired") == 0 || strcmp(event, "node.unpaired") == 0)) {
        muse_logf(&s->p->log, "the Muse removed this device");
        session_end(s, MUSE_OUTCOME_UNPAIRED);
        return;
    }
    if (cJSON_IsString(method) && strcmp(method->valuestring, "link.invoke") == 0) {
        on_invoke(s, message);
    }
}

/* MessageDecoder.feed: split the control stream into length-prefixed JSON. */
static void on_control_data(session *s, const uint8_t *data, size_t len)
{
    muse_buf *b = &s->control;
    if (muse_buf_append(b, data, len) != 0) {
        muse_logf(&s->p->log, "inbound message too large");
        session_end(s, MUSE_OUTCOME_CLOSED);
        return;
    }
    while (!s->ended && b->len >= 4) {
        size_t length = (size_t)b->data[0] | ((size_t)b->data[1] << 8) | ((size_t)b->data[2] << 16) |
                        ((size_t)b->data[3] << 24);
        if (length > MUSE_MAX_INBOUND_MESSAGE) {
            muse_logf(&s->p->log, "inbound message too large");
            session_end(s, MUSE_OUTCOME_CLOSED);
            return;
        }
        if (b->len < 4 + length) {
            break;
        }
        /* An empty message is a keepalive; cJSON yields NULL for it. */
        cJSON *message = cJSON_ParseWithLength((const char *)b->data + 4, length);
        muse_buf_consume(b, 4 + length);
        if (cJSON_IsObject(message)) {
            on_control_message(s, message);
        }
        cJSON_Delete(message);
    }
}

static void on_control_frame(session *s, const muse_frame *f)
{
    if (f->kind == MUSE_FRAME_RESET) {
        muse_logf(&s->p->log, "control stream reset: %s", f->reason);
        session_end(s, MUSE_OUTCOME_CLOSED);
        return;
    }
    if (f->kind == MUSE_FRAME_RESPONSE && f->status >= 400) {
        muse_logf(&s->p->log, "/link-control refused: HTTP %d", f->status);
        session_end(s, f->status == 403 ? MUSE_OUTCOME_FORBIDDEN : MUSE_OUTCOME_CLOSED);
        return;
    }
    on_control_data(s, f->data, f->data_len);
    if (f->end_body && !s->ended) {
        muse_logf(&s->p->log, "control stream ended by VM");
        session_end(s, MUSE_OUTCOME_CLOSED);
    }
}

static void on_ws_message(session *s)
{
    bool complete = false;
    int rc = muse_noise_receive(s->noise, s->rx.data, s->rx.len, &s->assembled, &complete);
    muse_frame f;
    if (rc == 0 && complete) {
        rc = muse_noise_decode_frame(s->assembled.data, s->assembled.len, &f);
    } else if (rc == 0) {
        return;
    }
    if (rc < 0) {
        muse_logf(&s->p->log, "bad frame from the Muse: error %d", rc);
        session_end(s, MUSE_OUTCOME_CLOSED);
        return;
    }
    if (rc != 0) {
        return;
    }
    if (f.stream_id == s->control_id) {
        on_control_frame(s, &f);
    } else if (s->sub.open && f.stream_id == s->sub.id) {
        on_subscription_frame(s, &f);
    } else if (s->chat.ack.active && f.stream_id == s->chat.ack.id) {
        on_request_frame(s, &f);
    }
}

/* --- connection lifecycle ----------------------------------------------------- */

/* A read on a dead connection just times out, so a ping that draws no frame
 * of any kind is what ends the session. */
static void keepalive_step(session *s)
{
    keepalive *k = &s->keepalive;
    uint32_t now = muse_port_now_ms();
    uint32_t last_rx = muse_ws_last_rx_ms(s->ws);
    if (k->ping_pending) {
        if ((uint32_t)(last_rx - k->ping_sent_ms) < 0x80000000u) {
            k->ping_pending = false;
            k->cycle_start_ms = now;
            return;
        }
        if ((uint32_t)(now - k->ping_sent_ms) <= MUSE_PING_TIMEOUT_MS) {
            return;
        }
    } else {
        if ((uint32_t)(now - k->cycle_start_ms) < MUSE_PING_INTERVAL_MS) {
            return;
        }
        if ((uint32_t)(now - last_rx) <= MUSE_RX_TIMEOUT_MS) {
            outbound ping = {.kind = OUT_PING};
            k->ping_pending = true;
            k->ping_sent_ms = now;
            (void)session_send(s, &ping);
            return;
        }
    }
    muse_logf(&s->p->log, "the connection to the Muse went quiet; reconnecting");
    session_end(s, MUSE_OUTCOME_CLOSED);
}

static bool stop_requested(const session *s)
{
    return *s->p->stop_flag != 0;
}

static int handshake(session *s)
{
    uint8_t msg1[32];
    int rc = muse_noise_write_message1(s->noise, msg1);
    if (rc != 0) {
        return rc;
    }
    outbound first = {.kind = OUT_RAW, .data = msg1, .len = sizeof msg1};
    rc = session_send(s, &first);
    if (rc != 0) {
        return rc;
    }

    uint32_t started = muse_port_now_ms();
    int opcode = 0;
    for (;;) {
        uint32_t elapsed = (uint32_t)(muse_port_now_ms() - started);
        if (elapsed >= MUSE_HANDSHAKE_TIMEOUT_MS) {
            return MUSE_ETIMEOUT;
        }
        if (stop_requested(s)) {
            session_end(s, MUSE_OUTCOME_STOPPED);
            return MUSE_EIO;
        }
        uint32_t left = MUSE_HANDSHAKE_TIMEOUT_MS - elapsed;
        rc = muse_ws_recv(s->ws, left < MUSE_READ_SLICE_MS ? left : MUSE_READ_SLICE_MS, &opcode,
                          &s->rx);
        if (rc < 0) {
            return rc;
        }
        if (rc == 1) {
            break;
        }
    }
    if (opcode != MUSE_WS_BINARY) {
        muse_logf(&s->p->log, "Noise handshake got a text frame");
        return MUSE_EPROTO;
    }
    rc = muse_noise_read_message2(s->noise, s->rx.data, s->rx.len);
    if (rc != 0) {
        return rc;
    }
    muse_buf msg3;
    muse_buf_init(&msg3, 128);
    rc = muse_noise_write_message3(s->noise, &msg3);
    if (rc == 0) {
        outbound third = {.kind = OUT_RAW, .data = msg3.data, .len = msg3.len};
        rc = session_send(s, &third);
    }
    muse_buf_free(&msg3);
    if (rc == 0) {
        rc = muse_noise_split(s->noise);
    }
    if (rc == 0) {
        muse_logf(&s->p->log, "Noise session established");
    }
    return rc;
}

static int open_control_stream(session *s)
{
    outbound open = {.kind = OUT_REQUEST, .path = CONTROL_PATH};
    int rc = session_send(s, &open);
    if (rc != 0) {
        return rc;
    }
    s->control_id = open.stream_id;
    if (uuid4(s, s->register_id) != 0) {
        return MUSE_EIO;
    }
    cJSON *params = cJSON_Parse(s->p->register_json);
    cJSON *message = cJSON_CreateObject();
    rc = MUSE_ENOMEM;
    if (params != NULL && message != NULL &&
        cJSON_AddStringToObject(message, "type", "req") != NULL &&
        cJSON_AddStringToObject(message, "id", s->register_id) != NULL &&
        cJSON_AddStringToObject(message, "method", "link.register") != NULL) {
        cJSON_AddItemToObject(message, "params", params);
        params = NULL;
        rc = send_control_message(s, message);
    }
    cJSON_Delete(params);
    cJSON_Delete(message);
    if (rc == 0) {
        muse_logf(&s->p->log, "sent link.register as %s", s->p->node_id);
    }
    return rc;
}

static int connect_ws(session *s)
{
    static const char scheme[] = "Bearer ";
    size_t size = sizeof scheme + strlen(s->p->vm_auth_token);
    char *bearer = malloc(size);
    if (bearer == NULL) {
        return MUSE_ENOMEM;
    }
    snprintf(bearer, size, "%s%s", scheme, s->p->vm_auth_token);
    const muse_header headers[] = {{"Authorization", bearer}};
    int rc = muse_ws_connect(s->p->tls, s->p->allow_plaintext, s->p->noise_url, headers, 1,
                             MUSE_WS_CONNECT_TIMEOUT_MS, &s->ws);
    memset(bearer, 0, size);
    free(bearer);
    return rc;
}

/* Everything up to the registration request. A failure leaves the outcome
 * set and returns nonzero. */
static int session_open(session *s)
{
    /* Made before connecting: the keys are slow on a small CPU and the
     * server's handshake clock starts at the upgrade. */
    int rc = muse_noise_new(s->p->tls, &s->noise);
    if (rc != 0) {
        muse_logf(&s->p->log, "session failed: no Noise keys (error %d)", rc);
        return rc;
    }
    rc = connect_ws(s);
    if (rc == MUSE_EAUTH || rc == MUSE_EFORBIDDEN || rc == MUSE_EHTTP) {
        muse_logf(&s->p->log, "VM refused connection%s",
                  rc == MUSE_EAUTH ? ": HTTP 401" : rc == MUSE_EFORBIDDEN ? ": HTTP 403" : "");
        session_end(s, rc == MUSE_EAUTH ? MUSE_OUTCOME_AUTH_REJECTED : MUSE_OUTCOME_FORBIDDEN);
        return rc;
    }
    if (rc == 0) {
        rc = handshake(s);
    }
    if (rc == 0) {
        rc = open_control_stream(s);
    }
    if (rc != 0 && !s->ended) {
        muse_logf(&s->p->log, "session failed: error %d", rc);
    }
    return rc;
}

static void serve(session *s)
{
    s->keepalive.cycle_start_ms = muse_port_now_ms();
    while (!s->ended) {
        int opcode = 0;
        int rc = muse_ws_recv(s->ws, MUSE_READ_SLICE_MS, &opcode, &s->rx);
        if (rc < 0) {
            muse_logf(&s->p->log, "control connection closed: error %d", rc);
            session_end(s, MUSE_OUTCOME_CLOSED);
        } else if (rc == 1 && opcode == MUSE_WS_BINARY) {
            on_ws_message(s);
        }
        if (stop_requested(s)) {
            session_end(s, MUSE_OUTCOME_STOPPED);
        }
        if (!s->ended) {
            keepalive_step(s);
        }
        if (!s->ended) {
            chat_step(s);
        }
    }
}

muse_outcome muse_link_run(const muse_link_params *p)
{
    session *s = calloc(1, sizeof *s);
    if (s == NULL) {
        return MUSE_OUTCOME_CLOSED;
    }
    s->p = p;
    s->outcome = MUSE_OUTCOME_CLOSED;
    muse_buf_init(&s->control, 4u + MUSE_MAX_INBOUND_MESSAGE);
    muse_buf_init(&s->rx, MUSE_MAX_WS_MESSAGE);
    muse_buf_init(&s->assembled, MUSE_MAX_ASSEMBLY_BYTES);
    muse_buf_init(&s->sub.line, MUSE_MAX_EVENT_LINE);

    if (session_open(s) == 0) {
        serve(s);
    }

    if (s->chat.phase != CHAT_IDLE) {
        chat_finish(s, "session ended");
    }
    muse_ws_close(s->ws);
    muse_noise_free(s->noise);
    muse_buf_free(&s->control);
    muse_buf_free(&s->rx);
    muse_buf_free(&s->assembled);
    muse_buf_free(&s->sub.line);
    muse_outcome outcome = s->outcome;
    free(s);
    return outcome;
}
