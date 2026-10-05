/* The library's service loop against tests/fake_muse_psp.py, over real
 * sockets.
 *
 *     host_session API_PORT VM_PORT [--voice]
 *
 * Prints the events it saw as one JSON array on the last line of stdout. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <cjson/cJSON.h>
#include <mbedtls/sha256.h>

#include "muse.h"
#include "muse_port.h"

#define CONNECT_WAIT_MS 15000u
#define REPLY_WAIT_MS 120000u
#define ASK_ATTEMPTS 4
#define WAV_BYTES (100u * 1024u)
#define WAV_HEADER 44u

typedef struct reply_state {
    bool final;
    bool failed;
    char *partial;   /* the last text delivered before the final one */
} reply_state;

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static cJSON *events;
static reply_state reply;
static int asks_done;
static bool run_over;
static bool driver_ok;
static muse *client;

static char *copy_string(const char *s)
{
    size_t n = strlen(s) + 1;
    char *copy = malloc(n);
    if (copy != NULL) {
        memcpy(copy, s, n);
    }
    return copy;
}

/* Appends [name, a, b] to the events; a or b NULL leaves that slot out. */
static void record(const char *name, const char *a, const char *b)
{
    cJSON *event = cJSON_CreateArray();
    cJSON_AddItemToArray(event, cJSON_CreateString(name));
    if (a != NULL) {
        cJSON_AddItemToArray(event, cJSON_CreateString(a));
    }
    if (b != NULL) {
        cJSON_AddItemToArray(event, cJSON_CreateString(b));
    }
    pthread_mutex_lock(&lock);
    cJSON_AddItemToArray(events, event);
    pthread_mutex_unlock(&lock);
}

static void on_log(void *user, const char *line)
{
    (void)user;
    record("log", line, NULL);
}

static void on_state(void *user, muse_state state)
{
    static const char *const names[] = {"unpaired", "connecting", "connected"};
    (void)user;
    record("state", names[state], NULL);
}

static void on_tokens(void *user, const char *access, const char *refresh, int64_t saved_at)
{
    (void)user;
    (void)saved_at;
    record("tokens", access, refresh);
}

static char *on_invoke(void *user, const char *command, const char *params_json)
{
    (void)user;
    cJSON *result = cJSON_CreateObject();
    cJSON *payload = cJSON_AddObjectToObject(result, "payload");
    cJSON_AddTrueToObject(result, "ok");

    if (strcmp(command, "test.progress") == 0) {
        pthread_mutex_lock(&lock);
        cJSON_AddNumberToObject(payload, "asks_done", asks_done);
        pthread_mutex_unlock(&lock);
    } else {
        cJSON *params = cJSON_Parse(params_json);
        const cJSON *text = cJSON_GetObjectItemCaseSensitive(params, "text");
        const char *value = cJSON_IsString(text) ? text->valuestring : "";

        /* The library does not pass link.invoke's timeout_ms on, so the
         * slot device_session.py fills with it is always null here. */
        cJSON *event = cJSON_CreateArray();
        cJSON_AddItemToArray(event, cJSON_CreateString("invoke"));
        cJSON_AddItemToArray(event, cJSON_CreateString(command));
        cJSON_AddItemToArray(event, cJSON_CreateNumber((double)strlen(value)));
        cJSON_AddItemToArray(event, cJSON_CreateNull());
        pthread_mutex_lock(&lock);
        cJSON_AddItemToArray(events, event);
        pthread_mutex_unlock(&lock);

        cJSON_AddStringToObject(payload, strcmp(command, "echo") == 0 ? "text" : "shown", value);
        cJSON_Delete(params);
    }
    char *printed = cJSON_PrintUnformatted(result);
    cJSON_Delete(result);
    return printed;
}

/* Runs on the muse_run thread, so the ask events fall in order with the
 * state and invoke events around them. */
static void on_reply(void *user, const char *text, bool final, const char *error)
{
    (void)user;
    if (!final) {
        pthread_mutex_lock(&lock);
        free(reply.partial);
        reply.partial = copy_string(text);
        pthread_mutex_unlock(&lock);
        return;
    }
    pthread_mutex_lock(&lock);
    char *partial = reply.partial;
    reply.partial = NULL;
    pthread_mutex_unlock(&lock);
    if (error != NULL) {
        record("ask_failed", error, NULL);
    } else {
        record("ask", text, partial != NULL ? partial : "");
    }
    free(partial);
    pthread_mutex_lock(&lock);
    if (error == NULL) {
        asks_done++;
    }
    reply.failed = error != NULL;
    reply.final = true;
    pthread_mutex_unlock(&lock);
}

static bool is_run_over(void)
{
    pthread_mutex_lock(&lock);
    bool over = run_over;
    pthread_mutex_unlock(&lock);
    return over;
}

static bool wait_connected(void)
{
    uint32_t started = muse_port_now_ms();
    while ((uint32_t)(muse_port_now_ms() - started) < CONNECT_WAIT_MS && !is_run_over()) {
        if (muse_get_state(client) == MUSE_STATE_CONNECTED) {
            return true;
        }
        muse_port_sleep_ms(50);
    }
    return false;
}

/* Sixteen samples of one sine period, so the WAV needs no libm. */
static uint8_t *make_wav(void)
{
    static const int16_t period[16] = {0, 4592, 8485, 11087, 12000, 11087, 8485, 4592,
                                       0, -4592, -8485, -11087, -12000, -11087, -8485, -4592};
    static const uint8_t header[WAV_HEADER] = {
        'R', 'I', 'F', 'F', 0xF8, 0x8F, 0x01, 0x00, 'W', 'A', 'V', 'E', 'f', 'm', 't', ' ',
        16, 0, 0, 0, 1, 0, 1, 0, 0x80, 0x3E, 0, 0, 0x00, 0x7D, 0, 0, 2, 0, 16, 0,
        'd', 'a', 't', 'a', 0xD4, 0x8F, 0x01, 0x00,
    };
    uint8_t *wav = malloc(WAV_BYTES);
    if (wav == NULL) {
        return NULL;
    }
    memcpy(wav, header, sizeof header);
    for (size_t i = 0; WAV_HEADER + 2 * i + 1 < WAV_BYTES; i++) {
        uint16_t sample = (uint16_t)period[i % 16];
        wav[WAV_HEADER + 2 * i] = (uint8_t)(sample & 0xFF);
        wav[WAV_HEADER + 2 * i + 1] = (uint8_t)(sample >> 8);
    }
    return wav;
}

static void record_voice(const uint8_t *wav)
{
    static const char hex[] = "0123456789abcdef";
    unsigned char digest[32];
    char text[65];
    mbedtls_sha256_ret(wav, WAV_BYTES, digest, 0);
    for (size_t i = 0; i < sizeof digest; i++) {
        text[2 * i] = hex[digest[i] >> 4];
        text[2 * i + 1] = hex[digest[i] & 15];
    }
    text[64] = '\0';
    cJSON *event = cJSON_CreateArray();
    cJSON_AddItemToArray(event, cJSON_CreateString("voice"));
    cJSON_AddItemToArray(event, cJSON_CreateNumber(WAV_BYTES));
    cJSON_AddItemToArray(event, cJSON_CreateString(text));
    pthread_mutex_lock(&lock);
    cJSON_AddItemToArray(events, event);
    pthread_mutex_unlock(&lock);
}

static int submit(const char *text, const uint8_t *wav)
{
    return wav != NULL ? muse_ask_voice(client, wav, WAV_BYTES) : muse_ask_text(client, text);
}

/* One chat turn. A turn that fails because the session dropped is retried on
 * the next session, which is what the --dead run exercises. */
static bool ask(const char *text, const uint8_t *wav)
{
    for (int attempt = 0; attempt < ASK_ATTEMPTS; attempt++) {
        if (!wait_connected()) {
            return false;
        }
        pthread_mutex_lock(&lock);
        reply.final = false;
        free(reply.partial);
        reply.partial = NULL;
        pthread_mutex_unlock(&lock);

        int rc = submit(text, wav);
        /* The library clears its pending flag just after the final
         * callback, so the next ask can be a moment early. */
        for (int spin = 0; rc == MUSE_EBUSY && spin < 100; spin++) {
            muse_port_sleep_ms(10);
            rc = submit(text, wav);
        }
        if (rc == MUSE_EOFFLINE) {
            muse_port_sleep_ms(100);
            continue;
        }
        if (rc != MUSE_OK) {
            return false;
        }

        uint32_t started = muse_port_now_ms();
        bool final = false;
        while (!final && (uint32_t)(muse_port_now_ms() - started) < REPLY_WAIT_MS) {
            muse_port_sleep_ms(20);
            pthread_mutex_lock(&lock);
            final = reply.final;
            pthread_mutex_unlock(&lock);
        }
        if (!final) {
            return false;
        }
        pthread_mutex_lock(&lock);
        bool failed = reply.failed;
        pthread_mutex_unlock(&lock);
        if (!failed) {
            return true;
        }
    }
    return false;
}

static void *driver(void *arg)
{
    bool voice = *(bool *)arg;
    bool ok = ask("what is up", NULL) && ask("and now", NULL);
    if (ok && voice) {
        uint8_t *wav = make_wav();
        ok = wav != NULL;
        if (ok) {
            record_voice(wav);
            ok = ask(NULL, wav);
        }
        free(wav);
    }
    pthread_mutex_lock(&lock);
    driver_ok = ok;
    pthread_mutex_unlock(&lock);
    if (!ok) {
        /* The fake would wait for turns that are not coming. */
        muse_stop(client);
    }
    return NULL;
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: host_session API_PORT VM_PORT [--voice]\n");
        return 2;
    }
    bool voice = argc > 3 && strcmp(argv[3], "--voice") == 0;
    char api_root[64];
    char noise_host[64];
    snprintf(api_root, sizeof api_root, "http://127.0.0.1:%d", atoi(argv[1]));
    snprintf(noise_host, sizeof noise_host, "ws://127.0.0.1:%d", atoi(argv[2]));
    events = cJSON_CreateArray();

    muse_config config;
    memset(&config, 0, sizeof config);
    config.api_root = api_root;
    config.noise_host = noise_host;
    config.access_token = "old-access";
    config.refresh_token = "hatch_refresh:r1";
    config.access_token_saved_at = 0;
    config.node_id = "homelink-010203";
    config.display_name = "Test Badge";
    config.sdk_token = "mgst_test";
    config.commands_json = "{\"badge.show_message\":{}}";
    config.version = "0.1.0";
    config.user_agent = "musebadge-test";
    config.network_ssid = "TestNet";
    config.allow_plaintext = true;
    config.callbacks.on_state = on_state;
    config.callbacks.on_tokens = on_tokens;
    config.callbacks.on_invoke = on_invoke;
    config.callbacks.on_reply = on_reply;
    config.callbacks.on_log = on_log;

    client = muse_new(&config);
    if (client == NULL) {
        fprintf(stderr, "muse_new failed\n");
        return 1;
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, driver, &voice) != 0) {
        fprintf(stderr, "pthread_create failed\n");
        return 1;
    }
    muse_run_end end = muse_run(client);
    pthread_mutex_lock(&lock);
    run_over = true;
    pthread_mutex_unlock(&lock);
    pthread_join(thread, NULL);
    muse_free(client);

    char *printed = cJSON_PrintUnformatted(events);
    printf("%s\n", printed);
    cJSON_free(printed);
    cJSON_Delete(events);
    free(reply.partial);
    return end == MUSE_RUN_UNPAIRED && driver_ok ? 0 : 1;
}
