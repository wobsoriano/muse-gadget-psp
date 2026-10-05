/* Keeps a paired device connected to its Muse. Ported from the Muse Gadget
 * SDK's service.py by way of musebadge/service.py: each round rotates the
 * device token when due, fetches the leased VM, serves one session, then
 * backs off. Pairing itself lives outside the library, so an unpaired device
 * ends the run. */
#include "muse_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <cjson/cJSON.h>
#include <mbedtls/platform_util.h>

#define DEFAULT_API_ROOT "https://api.muse.ai"
#define DEFAULT_NOISE_HOST "hatch.metaaivm.com"
#define NOISE_PATH "/v1/noise"
#define BACKOFF_BASE_S 2u
#define BACKOFF_MAX_S 60u
#define AUTH_BACKOFF_MIN_S 15u
#define HEALTHY_SESSION_S 30u
/* Device access tokens live about 4 hours; rotate at 3. */
#define TOKEN_REFRESH_AGE_S (3 * 3600)
#define TOKEN_RETRY_S 300u
#define STOP_POLL_MS 100u
#define MAX_ASK_TEXT (16u * 1024u)

struct muse {
    char *api_root;
    char *noise_host;
    char *access_token;
    char *refresh_token;
    int64_t access_token_saved_at;
    char *node_id;
    char *sdk_token;
    char *user_agent;
    char *register_json;
    bool allow_plaintext;
    muse_callbacks cb;
    muse_callbacks link_cb;   /* what a session calls; on_state also records the state */
    muse_log log;
    muse_tls tls;

    bool refresh_attempted;
    uint32_t last_refresh_attempt_ms;
    /* The SDK token reaches Muse only in refresh bodies, so each start with
     * a token attempts one refresh to report it. */
    bool sdk_token_reported;

    muse_mutex *lock;         /* guards state, ask and ask_pending */
    volatile int stop_flag;
    volatile int ask_pending;
    muse_ask ask;
    muse_state state;
};

typedef enum refresh_result {
    REFRESH_PROCEED,   /* use the tokens held now */
    REFRESH_RETRY,     /* a forced refresh failed; the old token is known bad */
    REFRESH_REVOKED    /* the pairing is gone */
} refresh_result;

static const char *const OUTCOME_NAMES[] = {
    [MUSE_OUTCOME_CLOSED] = "closed",
    [MUSE_OUTCOME_AUTH_REJECTED] = "auth_rejected",
    [MUSE_OUTCOME_FORBIDDEN] = "forbidden",
    [MUSE_OUTCOME_UNPAIRED] = "unpaired",
    [MUSE_OUTCOME_STOPPED] = "stopped",
};

static void set_state(muse *m, muse_state state)
{
    muse_port_mutex_lock(m->lock);
    bool changed = m->state != state;
    m->state = state;
    muse_port_mutex_unlock(m->lock);
    if (changed && m->cb.on_state != NULL) {
        m->cb.on_state(m->cb.user, state);
    }
}

static void link_on_state(void *user, muse_state state)
{
    set_state((muse *)user, state);
}

static char *link_on_invoke(void *user, const char *command, const char *params_json)
{
    muse *m = user;
    return m->cb.on_invoke != NULL ? m->cb.on_invoke(m->cb.user, command, params_json) : NULL;
}

static void link_on_settled(void *user, const char *text)
{
    muse *m = user;
    if (m->cb.on_settled != NULL) {
        m->cb.on_settled(m->cb.user, text);
    }
}

static void link_on_reply(void *user, const char *text, bool final, const char *error)
{
    muse *m = user;
    if (m->cb.on_reply != NULL) {
        m->cb.on_reply(m->cb.user, text, final, error);
    }
}

static void wipe_free(char *secret)
{
    if (secret != NULL) {
        mbedtls_platform_zeroize(secret, strlen(secret));
        free(secret);
    }
}

/* An ask accepted just before the session went away never reached it. */
static void fail_queued_ask(muse *m)
{
    muse_port_mutex_lock(m->lock);
    bool queued = m->ask_pending && m->ask.kind != MUSE_ASK_NONE;
    if (queued) {
        free(m->ask.data);
        m->ask.data = NULL;
        m->ask.len = 0;
        m->ask.kind = MUSE_ASK_NONE;
    }
    muse_port_mutex_unlock(m->lock);
    if (!queued) {
        return;
    }
    if (m->cb.on_reply != NULL) {
        m->cb.on_reply(m->cb.user, "", true, "not connected");
    }
    muse_port_mutex_lock(m->lock);
    m->ask_pending = 0;
    muse_port_mutex_unlock(m->lock);
}

static void leave_session(muse *m, muse_state state)
{
    set_state(m, state);
    fail_queued_ask(m);
}

static char *strip_trailing_slashes(const char *root)
{
    char *copy = muse_strdup(root);
    if (copy != NULL) {
        size_t len = strlen(copy);
        while (len > 0 && copy[len - 1] == '/') {
            copy[--len] = '\0';
        }
    }
    return copy;
}

/* register_params() plus the network name, as _register_with_network(). */
static char *build_register_json(const muse_config *config)
{
    cJSON *params = cJSON_CreateObject();
    cJSON *commands = cJSON_Parse(config->commands_json != NULL ? config->commands_json : "{}");
    char *text = NULL;
    bool ok = params != NULL && cJSON_IsObject(commands) &&
              cJSON_AddStringToObject(params, "node_id", config->node_id) != NULL &&
              cJSON_AddStringToObject(params, "display_name", config->display_name) != NULL &&
              /* The VM knows these values from the Linux Device SDK. Family
               * "link" must never be used: the server pushes ESP32 firmware
               * updates to every link device. */
              cJSON_AddStringToObject(params, "platform", "linux") != NULL &&
              cJSON_AddStringToObject(params, "version", config->version) != NULL &&
              cJSON_AddStringToObject(params, "device_family", "homehub") != NULL &&
              cJSON_AddStringToObject(params, "model_id", "linux") != NULL &&
              cJSON_AddFalseToObject(params, "is_wakeup_supported") != NULL;
    if (ok) {
        cJSON_AddItemToObject(params, "commands_v2", commands);
        commands = NULL;
        if (config->network_ssid != NULL && config->network_ssid[0] != '\0') {
            cJSON *metadata = cJSON_AddObjectToObject(params, "metadata");
            ok = metadata != NULL &&
                 cJSON_AddStringToObject(metadata, "network_ssid", config->network_ssid) != NULL;
        }
    }
    if (ok) {
        text = cJSON_PrintUnformatted(params);
    }
    cJSON_Delete(commands);
    cJSON_Delete(params);
    return text;
}

muse *muse_new(const muse_config *config)
{
    if (config == NULL || config->access_token == NULL || config->refresh_token == NULL ||
        config->node_id == NULL || config->display_name == NULL || config->version == NULL ||
        config->user_agent == NULL) {
        return NULL;
    }
    muse *m = calloc(1, sizeof *m);
    if (m == NULL) {
        return NULL;
    }
    m->cb = config->callbacks;
    m->log.user = m->cb.user;
    m->log.fn = m->cb.on_log;
    m->link_cb.user = m;
    m->link_cb.on_state = link_on_state;
    m->link_cb.on_invoke = link_on_invoke;
    m->link_cb.on_reply = link_on_reply;
    m->link_cb.on_settled = link_on_settled;
    m->allow_plaintext = config->allow_plaintext;
    m->access_token_saved_at = config->access_token_saved_at;
    m->state = MUSE_STATE_CONNECTING;

    /* muse_tls_init leaves its contexts initialised even when it fails, so
     * muse_free can release them from here on. */
    bool ok = muse_tls_init(&m->tls, config->ignore_cert_dates, m->log) == 0;

    m->api_root = strip_trailing_slashes(config->api_root != NULL && config->api_root[0] != '\0'
                                             ? config->api_root
                                             : DEFAULT_API_ROOT);
    m->noise_host = muse_strdup(config->noise_host != NULL && config->noise_host[0] != '\0'
                                    ? config->noise_host
                                    : DEFAULT_NOISE_HOST);
    m->access_token = muse_strdup(config->access_token);
    m->refresh_token = muse_strdup(config->refresh_token);
    m->node_id = muse_strdup(config->node_id);
    m->user_agent = muse_strdup(config->user_agent);
    m->sdk_token = muse_strdup(config->sdk_token);
    m->register_json = build_register_json(config);
    m->lock = muse_port_mutex_new();

    ok = ok && m->api_root != NULL && m->noise_host != NULL && m->access_token != NULL &&
         m->refresh_token != NULL && m->node_id != NULL && m->user_agent != NULL &&
         (config->sdk_token == NULL || m->sdk_token != NULL) && m->register_json != NULL &&
         m->lock != NULL;
    if (!ok) {
        muse_free(m);
        return NULL;
    }
    return m;
}

void muse_free(muse *m)
{
    if (m == NULL) {
        return;
    }
    muse_tls_free(&m->tls);
    if (m->lock != NULL) {
        muse_port_mutex_free(m->lock);
    }
    wipe_free(m->access_token);
    wipe_free(m->refresh_token);
    wipe_free(m->sdk_token);
    free(m->ask.data);
    free(m->api_root);
    free(m->noise_host);
    free(m->node_id);
    free(m->user_agent);
    cJSON_free(m->register_json);
    free(m);
}

void muse_stop(muse *m)
{
    muse_port_mutex_lock(m->lock);
    m->stop_flag = 1;
    muse_port_mutex_unlock(m->lock);
}

muse_state muse_get_state(const muse *m)
{
    muse_port_mutex_lock(m->lock);
    muse_state state = m->state;
    muse_port_mutex_unlock(m->lock);
    return state;
}

static int queue_ask(muse *m, muse_ask_kind kind, const void *bytes, size_t len)
{
    /* One spare byte keeps text NUL terminated for the session. */
    uint8_t *copy = malloc(len + 1);
    if (copy == NULL) {
        return MUSE_ENOMEM;
    }
    memcpy(copy, bytes, len);
    copy[len] = '\0';

    int rc = MUSE_OK;
    muse_port_mutex_lock(m->lock);
    if (m->ask_pending) {
        rc = MUSE_EBUSY;
    } else if (m->state != MUSE_STATE_CONNECTED) {
        rc = MUSE_EOFFLINE;
    } else {
        m->ask.kind = kind;
        m->ask.data = copy;
        m->ask.len = len;
        m->ask_pending = 1;
    }
    muse_port_mutex_unlock(m->lock);
    if (rc != MUSE_OK) {
        free(copy);
    }
    return rc;
}

int muse_ask_text(muse *m, const char *text)
{
    if (m == NULL || text == NULL || text[0] == '\0') {
        return MUSE_EINVAL;
    }
    size_t len = strlen(text);
    if (len > MAX_ASK_TEXT) {
        return MUSE_ETOOBIG;
    }
    return queue_ask(m, MUSE_ASK_TEXT, text, len);
}

int muse_ask_voice(muse *m, const void *wav, size_t wav_len)
{
    if (m == NULL || wav == NULL || wav_len == 0) {
        return MUSE_EINVAL;
    }
    if (wav_len > MUSE_MAX_VOICE_WAV) {
        return MUSE_ETOOBIG;
    }
    return queue_ask(m, MUSE_ASK_VOICE, wav, wav_len);
}

/* Sleeps in short steps so muse_stop() is noticed quickly. Returns true when
 * a stop was requested. */
static bool sleep_or_stop(muse *m, uint32_t seconds)
{
    uint32_t started = muse_port_now_ms();
    uint32_t total_ms = seconds * 1000u;
    while (!m->stop_flag) {
        uint32_t elapsed = (uint32_t)(muse_port_now_ms() - started);
        if (elapsed >= total_ms) {
            return false;
        }
        uint32_t left = total_ms - elapsed;
        muse_port_sleep_ms(left < STOP_POLL_MS ? left : STOP_POLL_MS);
    }
    return true;
}

/* _maybe_refresh: rotate the tokens first if they are due. */
static refresh_result maybe_refresh(muse *m, bool force)
{
    int64_t age = muse_port_unix_time() - m->access_token_saved_at;
    bool report_due = m->sdk_token != NULL && m->sdk_token[0] != '\0' && !m->sdk_token_reported;
    /* A clock that has not been set yet makes the age meaningless. */
    bool due = force || m->access_token_saved_at <= 0 || age >= TOKEN_REFRESH_AGE_S || age < 0;
    if (!due && !report_due) {
        return REFRESH_PROCEED;
    }
    uint32_t now = muse_port_now_ms();
    if (!force && m->refresh_attempted &&
        (uint32_t)(now - m->last_refresh_attempt_ms) < TOKEN_RETRY_S * 1000u) {
        return REFRESH_PROCEED;
    }
    m->refresh_attempted = true;
    m->last_refresh_attempt_ms = now;
    if (report_due) {
        m->sdk_token_reported = true;
        muse_logf(&m->log, "refreshing device token to report the SDK token");
    }

    int status = 0;
    char *access = NULL;
    char *refresh = NULL;
    int rc = muse_api_refresh_token(&m->tls, m->allow_plaintext, m->api_root, m->refresh_token,
                                    m->node_id, m->user_agent, m->sdk_token, m->log, &status,
                                    &access, &refresh);
    if (rc == 0) {
        wipe_free(m->access_token);
        wipe_free(m->refresh_token);
        m->access_token = access;
        m->refresh_token = refresh;
        m->access_token_saved_at = muse_port_unix_time();
        if (m->cb.on_tokens != NULL) {
            m->cb.on_tokens(m->cb.user, access, refresh, m->access_token_saved_at);
        }
        muse_logf(&m->log, "device token rotated");
        return REFRESH_PROCEED;
    }
    if (!due) {
        /* Only reporting the SDK token: nothing has rejected the current
         * token, so a refusal here must never unpair the device. */
        muse_logf(&m->log, "SDK token report refresh failed (HTTP %d); keeping the pairing", status);
        return REFRESH_PROCEED;
    }
    if (status == 401) {
        muse_logf(&m->log, "pairing revoked; set the device up again in the Muse app");
        return REFRESH_REVOKED;
    }
    /* Transient failure: keep using the current token while it still works. */
    return force ? REFRESH_RETRY : REFRESH_PROCEED;
}

static char *build_noise_url(const muse *m, const muse_vm *vm)
{
    const char *target = vm->vm_id[0] != '\0' ? vm->vm_id : vm->vm_name;
    size_t quoted_size = strlen(target) * 3u + 1u;
    char *quoted = malloc(quoted_size);
    muse_buf url;
    muse_buf_init(&url, 4096);
    bool ok = quoted != NULL && muse_url_encode_component(target, quoted, quoted_size) >= 0;
    if (ok && strstr(m->noise_host, "://") == NULL) {
        ok = muse_buf_append_str(&url, "wss://") == 0;
    }
    ok = ok && muse_buf_append_str(&url, m->noise_host) == 0 &&
         muse_buf_append_str(&url, NOISE_PATH "?vm_id=") == 0 &&
         muse_buf_append_str(&url, quoted) == 0;
    free(quoted);
    if (!ok) {
        muse_buf_free(&url);
        return NULL;
    }
    return muse_buf_take_str(&url);
}

/* _serve: one session with the chosen VM. *lasted_s is how long it stayed
 * registered. */
static muse_outcome serve(muse *m, const muse_vm *vm, uint32_t *lasted_s)
{
    *lasted_s = 0;
    char *url = build_noise_url(m, vm);
    if (url == NULL) {
        muse_logf(&m->log, "session failed: bad VM address");
        return MUSE_OUTCOME_CLOSED;
    }
    uint32_t registered_at_ms = 0;
    const muse_link_params params = {
        .tls = &m->tls,
        .allow_plaintext = m->allow_plaintext,
        .noise_url = url,
        .vm_auth_token = vm->vm_auth_token,
        .register_json = m->register_json,
        .node_id = m->node_id,
        .cb = &m->link_cb,
        .log = m->log,
        .lock = m->lock,
        .stop_flag = &m->stop_flag,
        .ask = &m->ask,
        .ask_pending = &m->ask_pending,
        .registered_at_ms = &registered_at_ms,
    };
    muse_logf(&m->log, "connecting to %s", vm->vm_name[0] != '\0' ? vm->vm_name : vm->vm_id);
    muse_outcome outcome = muse_link_run(&params);
    free(url);
    if (registered_at_ms != 0) {
        *lasted_s = (uint32_t)(muse_port_now_ms() - registered_at_ms) / 1000u;
    }
    muse_logf(&m->log, "session ended: %s after %us registered", OUTCOME_NAMES[outcome],
              (unsigned)*lasted_s);
    return outcome;
}

static muse_run_end end_unpaired(muse *m)
{
    leave_session(m, MUSE_STATE_UNPAIRED);
    return MUSE_RUN_UNPAIRED;
}

muse_run_end muse_run(muse *m)
{
    unsigned failures = 0;
    unsigned rejections = 0;
    uint32_t floor_s = 0;
    if (m->cb.on_state != NULL) {
        m->cb.on_state(m->cb.user, MUSE_STATE_CONNECTING);
    }
    while (!m->stop_flag) {
        leave_session(m, MUSE_STATE_CONNECTING);
        refresh_result refreshed = maybe_refresh(m, false);
        if (refreshed == REFRESH_REVOKED) {
            return end_unpaired(m);
        }

        muse_vm vm;
        int status = 0;
        int fetched = muse_api_fetch_default_vm(&m->tls, m->allow_plaintext, m->api_root,
                                                m->access_token, m->user_agent, m->log, &status,
                                                &vm);
        if (status == 401) {
            muse_logf(&m->log, "device token rejected by the API; refreshing");
            refreshed = maybe_refresh(m, true);
            if (refreshed == REFRESH_REVOKED) {
                return end_unpaired(m);
            }
            if (refreshed == REFRESH_RETRY && sleep_or_stop(m, TOKEN_RETRY_S)) {
                break;
            }
            /* The Python retries at once every time. A token that rotates
             * fine yet keeps being rejected would spin on the API, so only
             * the first rejection in a row skips the backoff. */
            if (rejections++ == 0) {
                continue;
            }
        } else {
            rejections = 0;
        }
        if (fetched == 0) {
            uint32_t lasted_s;
            muse_outcome outcome = serve(m, &vm, &lasted_s);
            muse_vm_free(&vm);
            if (m->stop_flag) {
                break;
            }
            if (outcome == MUSE_OUTCOME_UNPAIRED) {
                muse_logf(&m->log, "pairing removed by the Muse");
                return end_unpaired(m);
            }
            if (lasted_s >= HEALTHY_SESSION_S) {
                failures = 0;
                floor_s = 0;
            }
            if (outcome == MUSE_OUTCOME_AUTH_REJECTED || outcome == MUSE_OUTCOME_FORBIDDEN) {
                floor_s = AUTH_BACKOFF_MIN_S;
            }
        }
        leave_session(m, MUSE_STATE_CONNECTING);
        uint32_t delay_s = failures >= 5 ? BACKOFF_MAX_S : BACKOFF_BASE_S << failures;
        if (delay_s > BACKOFF_MAX_S) {
            delay_s = BACKOFF_MAX_S;
        }
        if (delay_s < floor_s) {
            delay_s = floor_s;
        }
        failures++;
        muse_logf(&m->log, "reconnecting in %us", (unsigned)delay_s);
        if (sleep_or_stop(m, delay_s)) {
            break;
        }
    }
    leave_session(m, MUSE_STATE_CONNECTING);
    return MUSE_RUN_STOPPED;
}
