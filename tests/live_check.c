/* The C client against the real Muse, using a real pairing.
 *
 *   build-host/live_check STATE_DIR COMMANDS_JSON SDK_TOKEN_FILE [VOICE_WAV]
 *
 * STATE_DIR holds identity.json and pairing.json, as tools/pair.py writes
 * them. One identity holds one session, so quit Muse on the PSP first if it
 * uses the same pairing. Tokens rotate on connect, so the new ones are
 * written back to STATE_DIR/pairing.json at once. Copy that file to the
 * Memory Stick afterwards, or the PSP is left holding dead tokens.
 */
#include <cjson/cJSON.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "muse.h"

static char pairing_path[512];
static volatile int replies_done;
static volatile int failed;

static char *slurp(const char *path, size_t *length)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        return NULL;
    }
    fseek(f, 0, SEEK_END);
    long size = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *data = malloc((size_t)size + 1);
    size_t got = fread(data, 1, (size_t)size, f);
    fclose(f);
    data[got] = 0;
    if (length) {
        *length = got;
    }
    return data;
}

static void on_tokens(void *user, const char *access, const char *refresh, int64_t saved_at)
{
    (void)user;
    char *text = slurp(pairing_path, NULL);
    cJSON *pairing = text ? cJSON_Parse(text) : NULL;
    free(text);
    if (!pairing) {
        fprintf(stderr, "cannot reread pairing.json; tokens NOT saved\n");
        failed = 1;
        return;
    }
    cJSON_ReplaceItemInObject(pairing, "access_token", cJSON_CreateString(access));
    cJSON_ReplaceItemInObject(pairing, "refresh_token", cJSON_CreateString(refresh));
    cJSON_ReplaceItemInObject(pairing, "access_token_saved_at", cJSON_CreateNumber((double)saved_at));
    char *out = cJSON_PrintUnformatted(pairing);
    char tmp[540];
    snprintf(tmp, sizeof tmp, "%s.tmp", pairing_path);
    FILE *f = fopen(tmp, "w");
    /* Written beside the file and renamed, so a crash cannot leave half a pairing. */
    if (f && fputs(out, f) >= 0 && fclose(f) == 0 && rename(tmp, pairing_path) == 0) {
        printf("tokens rotated and saved\n");
    } else {
        fprintf(stderr, "could not save rotated tokens\n");
        failed = 1;
    }
    free(out);
    cJSON_Delete(pairing);
}

static void on_state(void *user, muse_state state)
{
    (void)user;
    printf("state %d\n", (int)state);
}

static char *on_invoke(void *user, const char *command, const char *params)
{
    (void)user;
    printf("invoke %s %s\n", command, params);
    return strdup("{\"ok\":true,\"payload\":{\"handled_by\":\"live_check\"}}");
}

static void on_reply(void *user, const char *text, bool final, const char *error)
{
    (void)user;
    if (final) {
        printf("reply: %s%s%s\n", text ? text : "", error ? " ERROR " : "", error ? error : "");
        if (error) {
            failed = 1;
        }
        replies_done++;
    }
}

static void on_log(void *user, const char *line)
{
    (void)user;
    printf("  %s\n", line);
}

static void *run(void *m)
{
    printf("run ended: %d\n", (int)muse_run(m));
    return NULL;
}

static const char *field(cJSON *object, const char *name)
{
    cJSON *item = cJSON_GetObjectItem(object, name);
    return cJSON_IsString(item) ? item->valuestring : "";
}

int main(int argc, char **argv)
{
    if (argc < 4) {
        fprintf(stderr, "usage: live_check STATE_DIR COMMANDS_JSON SDK_TOKEN_FILE [VOICE_WAV]\n");
        return 2;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    char path[512];
    snprintf(pairing_path, sizeof pairing_path, "%s/pairing.json", argv[1]);
    snprintf(path, sizeof path, "%s/identity.json", argv[1]);
    char *identity_text = slurp(path, NULL), *pairing_text = slurp(pairing_path, NULL);
    char *commands = slurp(argv[2], NULL), *sdk_token = slurp(argv[3], NULL);
    cJSON *identity = identity_text ? cJSON_Parse(identity_text) : NULL;
    cJSON *pairing = pairing_text ? cJSON_Parse(pairing_text) : NULL;
    if (!identity || !pairing || !commands) {
        fprintf(stderr, "cannot read the pairing\n");
        return 2;
    }
    if (sdk_token) {
        sdk_token[strcspn(sdk_token, "\r\n")] = 0;
    }

    /* The node id is "homelink-" plus the last six hex digits of the identity. */
    const char *mac = field(identity, "mac");
    char node_id[32] = "homelink-";
    for (const char *c = mac; *c; c++) {
        if (*c != ':') {
            size_t n = strlen(node_id);
            node_id[n] = *c;
            node_id[n + 1] = 0;
        }
    }
    memmove(node_id + 9, node_id + strlen(node_id) - 6, 7);

    muse_config config = { 0 };
    config.api_root = field(pairing, "api_url_v2")[0] ? field(pairing, "api_url_v2") : NULL;
    config.noise_host = field(pairing, "noise_host")[0] ? field(pairing, "noise_host") : NULL;
    config.access_token = field(pairing, "access_token");
    config.refresh_token = field(pairing, "refresh_token");
    cJSON *saved = cJSON_GetObjectItem(pairing, "access_token_saved_at");
    config.access_token_saved_at = cJSON_IsNumber(saved) ? (int64_t)saved->valuedouble : 0;
    config.node_id = node_id;
    config.display_name = "PSP";
    config.sdk_token = sdk_token;
    config.commands_json = commands;
    config.version = "0.1.0";
    config.user_agent = "musegadget-c/0.1.0 (live check; macOS)";
    config.callbacks.on_state = on_state;
    config.callbacks.on_tokens = on_tokens;
    config.callbacks.on_invoke = on_invoke;
    config.callbacks.on_reply = on_reply;
    config.callbacks.on_log = on_log;
    printf("node %s\n", node_id);

    muse *m = muse_new(&config);
    pthread_t thread;
    pthread_create(&thread, NULL, run, m);
    for (int i = 0; i < 240 && muse_get_state(m) != MUSE_STATE_CONNECTED; i++) {
        usleep(250000);
    }
    if (muse_get_state(m) != MUSE_STATE_CONNECTED) {
        fprintf(stderr, "never connected\n");
        failed = 1;
    } else {
        sleep(2);
        printf("ask_text -> %d\n", muse_ask_text(
            m, "Connection test from the C client. Reply with only the word OK and use no tools."));
        for (int i = 0; i < 480 && replies_done < 1; i++) {
            usleep(250000);
        }
        if (argc > 4 && !failed) {
            size_t wav_len = 0;
            char *wav = slurp(argv[4], &wav_len);
            printf("ask_voice (%zu bytes) -> %d\n", wav_len, muse_ask_voice(m, wav, wav_len));
            free(wav);
            for (int i = 0; i < 480 && replies_done < 2; i++) {
                usleep(250000);
            }
        }
    }
    muse_stop(m);
    pthread_join(thread, NULL);
    muse_free(m);
    printf(failed ? "FAIL\n" : "PASS\n");
    return failed;
}
