#include "link.h"

#include <SDL2/SDL.h>
#include <cjson/cJSON.h>
#include <pspnet.h>
#include <pspnet_apctl.h>
#include <pspnet_inet.h>
#include <pspnet_resolver.h>
#include <psppower.h>
#include <psputility.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <time.h>

#include "muse.h"
#include "muse_psp.h"
#include "native_tts.h"

#define WIFI_PROFILE 1      /* the first saved connection in the PSP's Network Settings */
#define VERSION "0.1.0"
#define USER_AGENT "musegadget-c/" VERSION " (Sony PSP-3000)"
#define ENTROPY_WANTED 4096 /* bytes of microphone audio mixed in before any key is made */

#define PAIRING_FILE "state/pairing.json"
#define PAIRING_NEW "state/pairing.json.new"
#define IDENTITY_FILE "state/identity.json"
#define SDK_TOKEN_FILE "state/sdk_token.txt"
#define COMMANDS_FILE "commands.json"
#define SPEECH_KEY_FILE "state/openai_key.txt"
#define VOICE_FILE "voice.json"
#define VOICE_CHOICE_FILE "voice_choice.txt"
#define NATIVE_VOICE_FOLDER "tts"
#define SPEECH_RATE 24000                       /* what the speech service sends */
#define SPEECH_MAX_SAMPLES (LINK_AUDIO_RATE * 33) /* what the PSP keeps of one answer */
#define SPEECH_MAX_CHARS 420                    /* keeps a long answer under that */
#define SPEECH_HEAD_START (LINK_AUDIO_RATE / 4) /* buffered before playing starts */
#define SPEECH_WARM_MS 20000 /* the service drops a connection left idle much longer */

typedef struct frame {
    struct frame *next;
    char type;
    int length;
    char *payload;
} frame_t;

static volatile link_state_t state = LINK_NO_WIFI;
static SDL_mutex *lock;
static frame_t *inbox_head, *inbox_tail;
static muse *client;
static int module_results[2];

/* Each step goes to its own file and is closed at once. A crash here has
 * taken the whole PSP down before, and a buffered log loses exactly the last
 * lines that say where. */
static void trace(const char *format, ...)
{
    FILE *f = fopen("link_trace.txt", "a");
    if (f) {
        va_list args;
        fprintf(f, "%7u ", (unsigned)SDL_GetTicks()); /* milliseconds since the app started */
        va_start(args, format);
        vfprintf(f, format, args);
        va_end(args);
        fputc('\n', f);
        fclose(f);
    }
}

link_state_t link_state(void)
{
    return state;
}

/* Queues a frame that takes ownership of payload. */
static void push_owned(char type, char *payload, int length)
{
    frame_t *frame = malloc(sizeof(*frame));
    frame->type = type;
    frame->length = length;
    frame->payload = payload;
    frame->next = NULL;
    SDL_LockMutex(lock);
    if (inbox_tail) {
        inbox_tail->next = frame;
    } else {
        inbox_head = frame;
    }
    inbox_tail = frame;
    SDL_UnlockMutex(lock);
}

static void push(char type, const char *text)
{
    size_t length = strlen(text);
    char *copy = malloc(length + 1);
    memcpy(copy, text, length + 1);
    push_owned(type, copy, (int)length);
}

int link_next(char *type, char **payload, int *length)
{
    SDL_LockMutex(lock);
    frame_t *frame = inbox_head;
    if (frame) {
        inbox_head = frame->next;
        if (!inbox_head) {
            inbox_tail = NULL;
        }
    }
    SDL_UnlockMutex(lock);
    if (!frame) {
        return 0;
    }
    *type = frame->type;
    *payload = frame->payload;
    *length = frame->length;
    free(frame);
    return 1;
}

static void speech_warm(void);

int link_ask_voice(const void *wav, int length)
{
    if (!client || muse_ask_voice(client, wav, (size_t)length) != MUSE_OK) {
        return 0;
    }
    trace("question sent, %d bytes", length);
    speech_warm();
    return 1;
}

static char *slurp(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        return NULL;
    }
    fseek(f, 0, SEEK_END);
    long size = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *data = size >= 0 ? malloc((size_t)size + 1) : NULL;
    size_t got = data ? fread(data, 1, (size_t)size, f) : 0;
    fclose(f);
    if (data) {
        data[got] = 0;
    }
    return data;
}

static cJSON *load_pairing(void)
{
    /* A rotation is written to the .new file, then swapped in. If the PSP
     * lost power between those steps, the .new file is the live pairing. */
    char *text = slurp(PAIRING_NEW);
    cJSON *pairing = text ? cJSON_Parse(text) : NULL;
    free(text);
    if (pairing && cJSON_GetObjectItem(pairing, "refresh_token")) {
        trace("recovered the pairing from an interrupted save");
        return pairing;
    }
    cJSON_Delete(pairing);
    text = slurp(PAIRING_FILE);
    pairing = text ? cJSON_Parse(text) : NULL;
    free(text);
    return pairing;
}

static int reporting_sdk_token;

static void on_tokens(void *user, const char *access, const char *refresh, int64_t saved_at)
{
    cJSON *pairing = user;
    cJSON_ReplaceItemInObject(pairing, "access_token", cJSON_CreateString(access));
    cJSON_ReplaceItemInObject(pairing, "refresh_token", cJSON_CreateString(refresh));
    cJSON_ReplaceItemInObject(pairing, "access_token_saved_at", cJSON_CreateNumber((double)saved_at));
    if (reporting_sdk_token) {
        /* That rotation carried the SDK token, so later starts can skip it. */
        cJSON_DeleteItemFromObject(pairing, "sdk_token_reported");
        cJSON_AddTrueToObject(pairing, "sdk_token_reported");
    }
    char *out = cJSON_PrintUnformatted(pairing);
    FILE *f = fopen(PAIRING_NEW, "w");
    int ok = f && fputs(out, f) >= 0;
    if (f) {
        ok = fclose(f) == 0 && ok;
    }
    free(out);
    if (!ok) {
        /* The old tokens are dead. Without this save the next start is unpaired. */
        trace("COULD NOT SAVE ROTATED TOKENS");
        return;
    }
    remove(PAIRING_FILE);
    rename(PAIRING_NEW, PAIRING_FILE);
    trace("tokens rotated and saved");
}

static void on_state(void *user, muse_state now)
{
    (void)user;
    state = now == MUSE_STATE_CONNECTED ? LINK_UP : LINK_CONNECTING;
}

static const char *text_param(cJSON *params, const char *name)
{
    cJSON *item = cJSON_GetObjectItem(params, name);
    return cJSON_IsString(item) && item->valuestring[0] ? item->valuestring : NULL;
}

static void push_message(const char *title, const char *text)
{
    size_t size = strlen(title) + strlen(text) + 2;
    char *joined = malloc(size);
    snprintf(joined, size, "%s\n%s", title, text);
    push('M', joined);
    free(joined);
}

static void say(const char *text, int first_char);

static char *copy(const char *text)
{
    char *out = malloc(strlen(text) + 1);
    strcpy(out, text);
    return out;
}

/* What Muse gets when it asks how the PSP is doing. */
static char *status_report(void)
{
    cJSON *payload = cJSON_CreateObject();
    int percent = scePowerGetBatteryLifePercent(), minutes = scePowerGetBatteryLifeTime();
    if (scePowerIsBatteryExist() && percent >= 0) {
        cJSON_AddNumberToObject(payload, "battery_percent", percent);
        cJSON_AddBoolToObject(payload, "charging", scePowerIsBatteryCharging() > 0);
        if (minutes > 0) {
            cJSON_AddNumberToObject(payload, "battery_minutes_left", minutes);
        }
    } else {
        cJSON_AddStringToObject(payload, "battery", "none fitted");
    }
    cJSON_AddBoolToObject(payload, "plugged_in", scePowerIsPowerOnline() > 0);
    union SceNetApctlInfo info;
    if (sceNetApctlGetInfo(PSP_NET_APCTL_INFO_STRENGTH, &info) == 0) {
        cJSON_AddNumberToObject(payload, "wifi_signal_percent", info.strength);
    }
    if (sceNetApctlGetInfo(PSP_NET_APCTL_INFO_SSID, &info) == 0) {
        info.ssid[sizeof(info.ssid) - 1] = 0;
        cJSON_AddStringToObject(payload, "wifi_network", info.ssid);
    }
    cJSON_AddNumberToObject(payload, "cpu_mhz", scePowerGetCpuClockFrequency());
    cJSON_AddStringToObject(payload, "model", "Sony PSP-3000");
    cJSON *result = cJSON_CreateObject();
    cJSON_AddBoolToObject(result, "ok", 1);
    cJSON_AddItemToObject(result, "payload", payload);
    char *printed = cJSON_PrintUnformatted(result);
    cJSON_Delete(result);
    return printed;
}

static char *on_invoke(void *user, const char *command, const char *params_json)
{
    (void)user;
    cJSON *params = cJSON_Parse(params_json);
    char *result = NULL;
    if (strcmp(command, "psp.show_message") == 0) {
        const char *text = text_param(params, "text"), *title = text_param(params, "title");
        if (text) {
            push_message(title ? title : "Muse", text);
            say(text, 0);
            result = copy("{\"ok\":true,\"payload\":{\"shown\":true}}");
        } else {
            result = copy("{\"ok\":false,\"error\":\"text is required\"}");
        }
    } else if (strcmp(command, "psp.clear") == 0) {
        push('C', "");
        result = copy("{\"ok\":true,\"payload\":{\"cleared\":true}}");
    } else if (strcmp(command, "psp.dance") == 0) {
        cJSON *seconds = cJSON_GetObjectItem(params, "seconds");
        int n = cJSON_IsNumber(seconds) ? (int)seconds->valuedouble : 8;
        char text[8];
        snprintf(text, sizeof(text), "%d", n < 2 ? 2 : n > 60 ? 60 : n);
        push('D', text);
        result = copy("{\"ok\":true,\"payload\":{\"dancing\":true}}");
    } else if (strcmp(command, "psp.status") == 0) {
        result = status_report();
    } else {
        result = copy("{\"ok\":false,\"error\":\"unsupported command\"}");
    }
    cJSON_Delete(params);
    return result;
}

static void on_log(void *user, const char *line);

/* Speech. Muse answers in text only, so the words go to a text to speech
 * service and come back as audio. It runs on its own thread, so the
 * connection to Muse keeps being served, and the audio is handed to the
 * speaker while the rest is still downloading. */
static struct {
    SDL_sem *wake;
    char *pending;     /* the newest text waiting to be spoken, guarded by lock */
    int pending_first; /* where that text starts within what the screen shows */
    char *key;
    cJSON *voice;
    link_speech_t out;
    short *buffer;
    /* The download in progress. */
    volatile int warm_wanted; /* a question went out, so an answer is coming */
    muse_https *warm;  /* a connection opened ahead of the answer */
    Uint32 warm_at;
    int native;        /* the voice made on the PSP is loaded */
    int started;       /* the 'S' frame has gone out */
    int odd, odd_byte; /* half a sample left over from the last piece */
    int previous, phase, step; /* stepping from the source's rate to the speaker's */
} speech;

/* The voices on offer. A cloud voice names what the speech service calls
 * it. The last is made on the PSP itself, needs no key or network, and is
 * what any cloud voice falls back to when the service cannot be reached. */
static const struct {
    const char *label;
    const char *cloud;
} VOICES[] = {
    { "Alloy", "alloy" }, { "Coral", "coral" }, { "Nova", "nova" }, { "Sage", "sage" },
    { "Onyx", "onyx" },   { "Echo", "echo" },   { "Built in", NULL },
};
enum { VOICE_COUNT = sizeof(VOICES) / sizeof(VOICES[0]) };
static volatile int voice_choice;

int link_voice_count(void)
{
    return VOICE_COUNT;
}

const char *link_voice_label(int voice)
{
    return VOICES[voice].label;
}

int link_voice(void)
{
    return voice_choice;
}

const link_speech_t *link_speech(void)
{
    return &speech.out;
}

/* Cuts a long answer at the end of a sentence so the audio fits in memory. */
static void trim_for_speech(char *text)
{
    if (strlen(text) <= SPEECH_MAX_CHARS) {
        return;
    }
    size_t cut = SPEECH_MAX_CHARS;
    while (cut > 0 && ((unsigned char)text[cut] & 0xC0) == 0x80) {
        cut--; /* never split a UTF-8 character */
    }
    for (size_t i = cut; i > SPEECH_MAX_CHARS / 2; i--) {
        if (strchr(".!?", text[i - 1]) && text[i] == ' ') {
            cut = i;
            break;
        }
    }
    text[cut] = 0;
}

/* The PSP's speaker is small and the service leaves headroom, so the audio
 * is pushed up and whatever would clip is rounded off. */
static short louder(int sample)
{
    int v = sample * 3 / 2, mag = v < 0 ? -v : v;
    if (mag > 26000) {
        mag = 26000 + (int)((long long)(mag - 26000) * 6000 / (mag - 26000 + 6000));
    }
    return (short)(v < 0 ? -mag : mag);
}

static void speech_begin_playing(void)
{
    speech.started = 1;
    trace("speech: playing");
    push('S', "");
    push('T', "0");
}

/* Takes samples at the source's rate and steps them across to the
 * speaker's. Returns 1 when the buffer for one answer is full. */
static int speech_take(const short *samples, int count)
{
    int fill = speech.out.count, full = 0;
    for (int i = 0; i < count && !full; i++) {
        int sample = samples[i];
        for (; speech.phase < 65536; speech.phase += speech.step) {
            if (fill == SPEECH_MAX_SAMPLES) {
                full = 1;
                break;
            }
            speech.buffer[fill++] = louder(
                speech.previous + (int)((long long)(sample - speech.previous) * speech.phase >> 16));
        }
        speech.phase -= 65536;
        speech.previous = sample;
    }
    speech.out.count = fill;
    if (!speech.started && fill >= SPEECH_HEAD_START) {
        speech_begin_playing();
    }
    return full;
}

static void speech_source(int rate)
{
    speech.step = (int)(rate * 65536LL / LINK_AUDIO_RATE);
    speech.odd = speech.previous = speech.phase = 0;
}

/* One piece of the cloud service's response: bytes, two to a sample, cut
 * anywhere. */
static int on_speech_data(void *user, int status, const void *data, size_t len)
{
    (void)user;
    const unsigned char *bytes = data;
    if (status != 200) {
        trace("speech refused (%d): %.*s", status, (int)(len < 200 ? len : 200), (const char *)bytes);
        return 0;
    }
    short samples[256];
    int n = 0, full = 0;
    for (size_t i = 0; i < len && !full; i++) {
        if (!speech.odd) {
            speech.odd_byte = bytes[i];
            speech.odd = 1;
            continue;
        }
        speech.odd = 0;
        samples[n++] = (short)(speech.odd_byte | bytes[i] << 8);
        if (n == 256) {
            full = speech_take(samples, n);
            n = 0;
        }
    }
    return full || speech_take(samples, n);
}

static int on_native_samples(void *user, const short *samples, int count)
{
    (void)user;
    return speech_take(samples, count);
}

/* Asks the cloud service for `text` in one of its voices. Returns 1 when
 * any audio arrived. */
static int speak_from_cloud(const char *text, const char *voice)
{
    const char *url = text_param(speech.voice, "url");
    cJSON *request = cJSON_CreateObject();
    static const char *const fields[] = { "model", "instructions", "response_format" };
    for (int i = 0; i < 3; i++) {
        const char *value = text_param(speech.voice, fields[i]);
        if (value) {
            cJSON_AddStringToObject(request, fields[i], value);
        }
    }
    cJSON_AddStringToObject(request, "voice", voice);
    cJSON_AddStringToObject(request, "input", text);
    char *body = cJSON_PrintUnformatted(request);
    cJSON_Delete(request);

    int status = 0, rc = MUSE_EIO;
    Uint32 began = SDL_GetTicks();
    muse_https *connection = speech.warm;
    speech.warm = NULL;
    if (connection && began - speech.warm_at > SPEECH_WARM_MS) {
        muse_https_close(connection);
        connection = NULL;
    }
    speech_source(SPEECH_RATE);
    for (int attempt = 0; attempt < 2 && rc != MUSE_OK && speech.out.count == 0; attempt++) {
        int fresh = connection == NULL;
        if (fresh) {
            connection = muse_https_open(url, 15000, on_log, NULL);
        }
        trace("speech: asking for %d characters as %s on a %s connection", speech.out.chars, voice,
              fresh ? "new" : "waiting");
        rc = connection ? muse_https_post(connection, url, speech.key, "application/json", body,
                                          strlen(body), 40000, on_speech_data, NULL, &status)
                        : MUSE_EIO;
        connection = NULL;
        if (fresh) {
            break; /* only a connection that sat waiting earns a second try */
        }
    }
    free(body);
    trace("speech: rc %d, status %d, %d samples in %u ms", rc, status, speech.out.count,
          (unsigned)(SDL_GetTicks() - began));
    return speech.out.count > 0;
}

static int speech_worker(void *unused)
{
    (void)unused;
    for (;;) {
        SDL_SemWait(speech.wake);
        SDL_LockMutex(lock);
        char *text = speech.pending;
        int first_char = speech.pending_first;
        speech.pending = NULL;
        SDL_UnlockMutex(lock);
        const char *cloud = speech.key ? VOICES[voice_choice].cloud : NULL;
        if (!text) {
            /* The handshake is the slow part on a PSP, and Muse takes a few
             * seconds to answer, so it is done while waiting. */
            if (speech.warm_wanted && cloud) {
                muse_https_close(speech.warm);
                speech.warm = muse_https_open(text_param(speech.voice, "url"), 15000, on_log, NULL);
                speech.warm_at = SDL_GetTicks();
                trace("speech: connection %s", speech.warm ? "ready ahead of the answer" : "failed");
            }
            speech.warm_wanted = 0;
            continue;
        }
        speech.warm_wanted = 0;
        trim_for_speech(text);

        /* The speaker thread stops reading as soon as count drops, then
         * sees the generation change and starts over. */
        speech.out.done = 0;
        speech.out.count = 0;
        speech.out.first_char = first_char;
        speech.out.chars = (int)strlen(text);
        speech.out.generation++;
        speech.started = 0;

        if (!(cloud && speak_from_cloud(text, cloud)) && speech.native) {
            Uint32 began = SDL_GetTicks();
            speech_source(NATIVE_TTS_RATE);
            native_tts_speak(text, on_native_samples, NULL);
            trace("speech: made %d samples on the PSP in %u ms%s", speech.out.count,
                  (unsigned)(SDL_GetTicks() - began), cloud ? ", the cloud voice having failed" : "");
        }
        free(text);
        speech.out.done = 1;
        if (!speech.started) {
            if (speech.out.count > 0) {
                speech_begin_playing(); /* an answer shorter than the head start */
            } else {
                push('T', "0");
            }
        }
    }
    return 0;
}

static void speech_start(void)
{
    char *voice_text = slurp(VOICE_FILE), *chosen = slurp(VOICE_CHOICE_FILE);
    speech.key = slurp(SPEECH_KEY_FILE);
    speech.voice = voice_text ? cJSON_Parse(voice_text) : NULL;
    free(voice_text);
    if (speech.key) {
        speech.key[strcspn(speech.key, "\r\n")] = 0;
    }
    if (!speech.key || !speech.key[0] || !text_param(speech.voice, "url")) {
        trace("no speech key or voice settings, so no cloud voices");
        speech.key = NULL;
    }
    speech.native = native_tts_start(NATIVE_VOICE_FOLDER);
    trace("the voice made on the PSP is %s", speech.native ? "ready" : "not available");

    /* The voice picked last time, else the one voice.json names, else the first. */
    const char *wanted = chosen ? chosen : text_param(speech.voice, "voice");
    if (chosen) {
        chosen[strcspn(chosen, "\r\n")] = 0;
    }
    for (int i = 0; wanted && i < VOICE_COUNT; i++) {
        if (strcasecmp(wanted, VOICES[i].label) == 0) {
            voice_choice = i;
        }
    }
    free(chosen);

    speech.buffer = malloc(SPEECH_MAX_SAMPLES * sizeof(short));
    speech.out.samples = speech.buffer;
    speech.out.done = 1;
    if (!speech.buffer || (!speech.key && !speech.native)) {
        trace("no voice at all, answers will be text only");
        return;
    }
    speech.wake = SDL_CreateSemaphore(0);
    SDL_CreateThreadWithStackSize(speech_worker, "speech", 256 * 1024, NULL);
}

static void speech_warm(void)
{
    if (speech.wake) {
        speech.warm_wanted = 1;
        SDL_SemPost(speech.wake);
    }
}

/* Queues text to be spoken, replacing anything not yet started. */
static void say(const char *text, int first_char)
{
    if (!speech.wake || !text[0]) {
        return;
    }
    char *queued = copy(text);
    trace("speech: queued %u characters", (unsigned)strlen(text));
    SDL_LockMutex(lock);
    free(speech.pending);
    speech.pending = queued;
    speech.pending_first = first_char;
    SDL_UnlockMutex(lock);
    SDL_SemPost(speech.wake);
}

/* What this turn has already put on screen, so the confirmation that
 * follows a settled answer does not show and speak it twice. */
static char *answered;

/* Shows the answer so far and speaks whatever part is new. Returns 1 when
 * the speech thread will end the turn's busy state. */
static int answer(const char *text)
{
    if (answered && strcmp(answered, text) == 0) {
        return speech.wake != NULL;
    }
    size_t had = answered ? strlen(answered) : 0;
    int extends = had > 0 && had <= SPEECH_MAX_CHARS && strncmp(answered, text, had) == 0;
    push_message("Muse", text);
    if (!answered) {
        say(text, 0);
    } else if (extends) {
        size_t from = had + strspn(text + had, " \n");
        say(text + from, (int)from);
    }
    free(answered);
    answered = copy(text);
    return speech.wake != NULL && text[0];
}

static void on_settled(void *user, const char *text)
{
    (void)user;
    trace("answer settled, %u characters", (unsigned)strlen(text));
    answer(text);
}

static void on_reply(void *user, const char *text, bool final, const char *error)
{
    (void)user;
    if (!final) {
        return;
    }
    if (error) {
        trace("ask failed: %s", error);
        push_message("Muse", text && text[0] ? text : "Muse did not answer.");
        push('T', "0");
    } else if (!answer(text)) {
        push('T', "0");
    }
    free(answered);
    answered = NULL;
}

static void on_log(void *user, const char *line)
{
    (void)user;
    static int clock_warned;
    trace("muse: %s", line);
    /* A wrong clock makes every certificate look not yet valid or expired,
     * and from the screen that is indistinguishable from a slow connection. */
    if (!clock_warned && (strstr(line, "starts in the future") || strstr(line, "has expired"))) {
        clock_warned = 1;
        push_message("Check the clock", "The PSP's date looks wrong, so it cannot verify Muse's "
                     "certificate. Set it in Settings, Date & Time, then start Muse again.");
    }
}

static int join_wifi(void)
{
    int apctl = 0, last = -1;
    if (sceNetApctlConnect(WIFI_PROFILE) != 0) {
        return 0;
    }
    /* About 20 seconds to associate and get an address. */
    for (int i = 0; i < 400; i++) {
        if (sceNetApctlGetState(&apctl) != 0) {
            return 0;
        }
        if (apctl != last) {
            trace("wifi state %d", apctl);
            last = apctl;
        }
        if (apctl == PSP_NET_APCTL_STATE_GOT_IP) {
            return 1;
        }
        /* Dropping back to disconnected after starting means the join failed. */
        if (apctl == PSP_NET_APCTL_STATE_DISCONNECTED && i > 40) {
            return 0;
        }
        SDL_Delay(50);
    }
    return 0;
}

/* The Muse client retries forever on a dead network. Only this thread knows
 * the Wi-Fi itself has dropped, and stopping the client hands control back
 * to the loop that rejoins it. */
static int wifi_watch(void *unused)
{
    (void)unused;
    for (;;) {
        SDL_Delay(5000);
        int apctl = PSP_NET_APCTL_STATE_GOT_IP;
        if (client && sceNetApctlGetState(&apctl) == 0 && apctl != PSP_NET_APCTL_STATE_GOT_IP) {
            trace("wifi dropped (state %d); stopping the client to rejoin", apctl);
            muse_stop(client);
        }
    }
    return 0;
}

static int worker(void *unused)
{
    (void)unused;
    remove("link_trace.txt");
    cJSON *pairing = load_pairing();
    char *identity_text = slurp(IDENTITY_FILE), *commands = slurp(COMMANDS_FILE);
    char *sdk_token = slurp(SDK_TOKEN_FILE);
    cJSON *identity = identity_text ? cJSON_Parse(identity_text) : NULL;
    cJSON *mac = identity ? cJSON_GetObjectItem(identity, "mac") : NULL;
    if (!pairing || !cJSON_IsString(mac) || !commands) {
        trace("no pairing on the Memory Stick");
        state = LINK_UNPAIRED;
        return 0;
    }
    if (sdk_token) {
        sdk_token[strcspn(sdk_token, "\r\n")] = 0;
    }
    /* The node id is "homelink-" plus the last six hex digits of the identity. */
    char hex[16] = "", node_id[24];
    for (const char *c = mac->valuestring; *c && strlen(hex) < 12; c++) {
        if (*c != ':') {
            hex[strlen(hex)] = *c;
        }
    }
    snprintf(node_id, sizeof(node_id), "homelink-%s", strlen(hex) >= 6 ? hex + strlen(hex) - 6 : hex);

    /* Each step is traced by name: one of them failing leaves the app
     * looking like it has no Wi-Fi, and the code alone does not say which. */
    int steps[6] = {
        module_results[0],
        module_results[1],
        sceNetInit(128 * 1024, 42, 4 * 1024, 42, 4 * 1024),
        sceNetInetInit(),
        sceNetResolverInit(),
        sceNetApctlInit(0x8000, 48),
    };
    static const char *const names[6] = { "load common", "load inet", "net init", "inet init",
                                          "resolver init", "apctl init" };
    int result = 0;
    for (int i = 0; i < 6; i++) {
        trace("%s -> 0x%08x", names[i], steps[i]);
        result |= steps[i];
    }
    if (result != 0) {
        return 0;
    }

    /* The PSP has no hardware random source a program can reach, so keys are
     * not made until the audio thread has mixed in microphone noise. */
    while (muse_psp_entropy_added() < ENTROPY_WANTED) {
        SDL_Delay(50);
    }
    trace("entropy ready: %u bytes of microphone audio", (unsigned)muse_psp_entropy_added());
    trace("the C library's time() says %ld", (long)time(NULL));

    for (;;) {
        state = LINK_JOINING;
        trace("joining wifi");
        if (!join_wifi()) {
            trace("wifi join failed");
            state = LINK_NO_WIFI;
            sceNetApctlDisconnect();
            SDL_Delay(5000);
            continue;
        }
        state = LINK_CONNECTING;

        const char *api = text_param(pairing, "api_url_v2"), *host = text_param(pairing, "noise_host");
        cJSON *saved = cJSON_GetObjectItem(pairing, "access_token_saved_at");
        muse_config config = { 0 };
        config.api_root = api;
        config.noise_host = host;
        config.access_token = text_param(pairing, "access_token");
        config.refresh_token = text_param(pairing, "refresh_token");
        config.access_token_saved_at = cJSON_IsNumber(saved) ? (int64_t)saved->valuedouble : 0;
        config.node_id = node_id;
        config.display_name = "PSP";
        /* Reporting the SDK token costs a token rotation and a second
         * handshake at every start, and Muse only needs it once. */
        reporting_sdk_token = sdk_token && !cJSON_IsTrue(cJSON_GetObjectItem(pairing, "sdk_token_reported"));
        config.sdk_token = reporting_sdk_token ? sdk_token : NULL;
        config.commands_json = commands;
        config.version = VERSION;
        config.user_agent = USER_AGENT;
        config.callbacks.user = pairing;
        config.callbacks.on_state = on_state;
        config.callbacks.on_tokens = on_tokens;
        config.callbacks.on_invoke = on_invoke;
        config.callbacks.on_reply = on_reply;
        config.callbacks.on_settled = on_settled;
        config.callbacks.on_log = on_log;
        muse *m = muse_new(&config);
        if (!m) {
            trace("could not start the Muse client");
            state = LINK_UNPAIRED;
            return 0;
        }
        client = m;
        muse_run_end end = muse_run(m);
        client = NULL;
        muse_free(m);
        if (end == MUSE_RUN_UNPAIRED) {
            trace("the Muse removed this device");
            state = LINK_UNPAIRED;
            return 0;
        }
    }
}

void link_start(void)
{
    /* The system's network modules load into the same memory this app's
     * thread stacks come from. Loaded after the 256 KB connection thread
     * existed, the second one failed for lack of room, so they go first. */
    module_results[0] = sceUtilityLoadNetModule(PSP_NET_MODULE_COMMON);
    module_results[1] = sceUtilityLoadNetModule(PSP_NET_MODULE_INET);
    lock = SDL_CreateMutex();
    /* TLS handshakes and the Noise key agreement need far more stack than
     * SDL's default thread gets on the PSP. */
    SDL_CreateThreadWithStackSize(worker, "link", 256 * 1024, NULL);
    SDL_CreateThread(wifi_watch, "wifi", NULL);
    speech_start();
}

void link_set_voice(int voice)
{
    voice_choice = voice;
    FILE *f = fopen(VOICE_CHOICE_FILE, "w");
    if (f) {
        fputs(VOICES[voice].label, f);
        fclose(f);
    }
    trace("voice set to %s", VOICES[voice].label);
    say("Hi, I'm Muse. This is how I sound.", 0);
}
