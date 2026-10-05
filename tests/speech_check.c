/* Fetch speech the way the PSP does and write the raw PCM to a file.
 *
 *   speech_check <key file> <voice.json> <out.pcm> "text to say" [idle seconds]
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <cjson/cJSON.h>

#include "muse.h"

static void log_line(void *user, const char *line)
{
    (void)user;
    fprintf(stderr, "%s\n", line);
}

static FILE *out;
static size_t total;
static int pieces, first_ms;
static struct timespec started;

static int elapsed_ms(void)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (int)((now.tv_sec - started.tv_sec) * 1000 + (now.tv_nsec - started.tv_nsec) / 1000000);
}

static int on_data(void *user, int status, const void *data, size_t len)
{
    (void)user;
    if (pieces++ == 0) {
        first_ms = elapsed_ms();
    }
    if (status != 200) {
        fprintf(stderr, "%.*s\n", (int)len, (const char *)data);
        return 0;
    }
    total += len;
    return fwrite(data, 1, len, out) != len;
}

static char *slurp(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        return NULL;
    }
    char *data = calloc(1, 65536);
    size_t got = fread(data, 1, 65535, f);
    fclose(f);
    data[got] = 0;
    return data;
}

int main(int argc, char **argv)
{
    if (argc != 5 && argc != 6) {
        return 2;
    }
    char *key = slurp(argv[1]), *voice_text = slurp(argv[2]);
    cJSON *voice = voice_text ? cJSON_Parse(voice_text) : NULL;
    if (!key || !voice) {
        fprintf(stderr, "missing key or voice file\n");
        return 2;
    }
    key[strcspn(key, "\r\n")] = 0;
    cJSON *request = cJSON_CreateObject();
    static const char *const fields[] = {"model", "voice", "instructions", "response_format"};
    for (int i = 0; i < 4; i++) {
        cJSON_AddStringToObject(request, fields[i],
                                cJSON_GetObjectItem(voice, fields[i])->valuestring);
    }
    cJSON_AddStringToObject(request, "input", argv[4]);
    char *body = cJSON_PrintUnformatted(request);
    int status = 0;
    out = fopen(argv[3], "wb");
    clock_gettime(CLOCK_MONOTONIC, &started);
    const char *url = cJSON_GetObjectItem(voice, "url")->valuestring;
    muse_https *connection = muse_https_open(url, 20000, log_line, NULL);
    printf("handshake took %d ms\n", elapsed_ms());
    if (argc == 6) {
        sleep((unsigned)atoi(argv[5])); /* how long the PSP holds it while Muse thinks */
    }
    clock_gettime(CLOCK_MONOTONIC, &started);
    int rc = muse_https_post(connection, url, key, "application/json", body, strlen(body), 45000,
                             on_data, NULL, &status);
    fclose(out);
    printf("rc=%d status=%d bytes=%lu pieces=%d first byte after %d ms, all after %d ms\n", rc,
           status, (unsigned long)total, pieces, first_ms, elapsed_ms());
    return rc == 0 && status == 200 ? 0 : 1;
}
