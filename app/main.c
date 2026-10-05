/* Muse on the PSP: the avatar, its reactions, the message panel, and voice.
 *
 * The PSP is the Muse gadget itself: link.c holds the connection.
 *
 *   Hold R    talk. The recording goes to Muse as a voice note, or is played
 *             straight back when Muse is not reachable, as a microphone test.
 *   Circle    clear the message
 *   Cross     a sample message      Triangle  dance      (to demo the screen)
 *   Select    choose the voice
 *   Start     quit
 */
#include <SDL2/SDL.h>
#include <SDL2/SDL_image.h>
#include <SDL2/SDL_ttf.h>
#include <pspaudio.h>
#include <pspmoduleinfo.h>
#include <psppower.h>
#include <pspsysmem.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "clip_table.h"
#include "link.h"
#include "muse_psp.h"

/* Memory the C library's heap must leave alone. The system's network
 * modules and every thread's stack are carved from it, and with the default
 * the network modules did not fit once the Muse client was linked in. */
PSP_HEAP_THRESHOLD_SIZE_KB(6 * 1024);

#define SCREEN_W 480
#define SCREEN_H 272
#define PER_SHEET (CLIP_COLUMNS * CLIP_ROWS)
#define FEET_Y (SCREEN_H - 8)
#define PANEL_X 196
#define PANEL_W (SCREEN_W - PANEL_X - 16)
#define DANCE_MS 8000

/* PSP button numbers as SDL reports them on the joystick. */
enum { BTN_TRIANGLE, BTN_CIRCLE, BTN_CROSS, BTN_SQUARE, BTN_L, BTN_R,
       BTN_DOWN, BTN_LEFT, BTN_UP, BTN_RIGHT, BTN_SELECT, BTN_START };

static const struct { const char *label; SDL_Color dot; } LINKS[LINK_STATES] = {
    [LINK_UNPAIRED] = { "Not paired with Muse", { 232, 150, 60, 255 } },
    [LINK_NO_WIFI] = { "No Wi-Fi", { 232, 150, 60, 255 } },
    [LINK_JOINING] = { "Joining Wi-Fi", { 90, 130, 240, 255 } },
    [LINK_CONNECTING] = { "Connecting to Muse", { 90, 130, 240, 255 } },
    [LINK_UP] = { "Connected", { 70, 190, 120, 255 } },
};

static const struct { const char *title; const char *text; } SAMPLES[] = {
    { "Muse", "Your 2pm with Dana moved to 3. You have time for coffee." },
    { "Muse", "Why did the database admin leave his wife? She had one-to-many relationships." },
    { "Reminder", "Stand up and stretch. You have been at it for ninety minutes, and the build "
                  "will still be there when you get back." },
};

typedef struct {
    SDL_Texture *texture;
    int w, h;
} label_t;

/* A message laid out line by line, so a long answer can scroll along with
 * the voice. */
#define CAPTION_LINES 32
#define CAPTION_TOP 44

typedef struct {
    SDL_Texture *texture;
    TTF_Font *font;
    char *text;
    int line_h, lines;
    int start[CAPTION_LINES], end[CAPTION_LINES];   /* offsets into text */
} caption_t;

#define VOICE_ROWS 8

typedef struct {
    link_state_t link;
    int busy;
    int error;
    int has_message;
    Uint32 message_at;      /* when the last message arrived, for the bounce */
    Uint32 dance_until;
    Uint32 gesture_at;      /* when the clip started playing; 0 while resting */
    Uint32 next_gesture;
    float avatar_x;         /* eases between the centre and the left */
    label_t title, status, listening, playing;
    int picking;            /* the row under the cursor in the voice list, or -1 when it is closed */
    label_t picker_title, picker_hint, voices[VOICE_ROWS];
    caption_t body;
} app_t;

static SDL_Renderer *renderer;
static SDL_Texture *sheets[CLIP_SHEETS];
static int frames_loaded;
static SDL_Texture *glow;
static TTF_Font *font_title, *font_body[3], *font_small;

/* Voice: record while R is held, and play audio back.
 *
 * Recording and playback both block, so they run on their own thread and the
 * screen keeps animating. The main thread only writes `state` and the
 * playback source, and reads the rest.
 */
#define MIC_RATE LINK_AUDIO_RATE
#define MIC_CHUNK 512
#define MIC_MAX_SAMPLES (MIC_RATE * 12)
#define MIC_GAIN 0x1000

enum { CUE_NONE, CUE_START, CUE_STOP };

typedef enum { MIC_IDLE, MIC_RECORDING, MIC_RECORDED, MIC_PLAYING, MIC_DONE } mic_state_t;

static struct {
    volatile mic_state_t state;
    volatile int samples;       /* recorded so far */
    volatile int played;
    volatile int level;         /* peak of the latest chunk, 0..32767 */
    volatile int peak;          /* loudest sample of the take */
    volatile int error;         /* a negative PSP error code, or 0 */
    volatile int cue;           /* a CUE_ sound the audio thread owes the speaker */
    const link_speech_t *voice; /* what MIC_PLAYING plays: Muse's answer, or the take when NULL */
    short take[MIC_MAX_SAMPLES];
} mic;

/* The take as a WAV file at half the recording rate, which is plenty for
 * speech and halves what goes over the air. The caller frees it. */
static unsigned char *take_as_wav(int *length)
{
    int samples = mic.samples / 2;
    unsigned bytes = (unsigned)samples * 2, rate = MIC_RATE / 2;
    unsigned header[11] = { 0x46464952, 36 + bytes, 0x45564157, 0x20746d66, 16,
                            0x00010001, rate, rate * 2, 0x00100002, 0x61746164, bytes };
    unsigned char *wav = malloc(sizeof(header) + bytes);
    if (!wav) {
        return NULL;
    }
    memcpy(wav, header, sizeof(header));
    short *out = (short *)(wav + sizeof(header));
    /* The microphone's level varies a great deal from one take to the next
     * on a real PSP, so a quiet take is brought up before it is sent. */
    int peak = mic.peak > 600 ? mic.peak : 600;
    for (int i = 0; i < samples; i++) {
        int v = (mic.take[i * 2] + mic.take[i * 2 + 1]) / 2 * 24000 / peak;
        out[i] = (short)(v > 32767 ? 32767 : v < -32768 ? -32768 : v);
    }
    *length = (int)(sizeof(header) + bytes);
    return wav;
}

/* Between recordings the microphone is kept running and what it hears is
 * thrown away. On a real PSP the first recording after startup came back
 * dead (quieter than the empty room had been a moment before) while every
 * later one was fine, whatever the speaker had done in between. A microphone
 * that never stops has no first recording.
 *
 * The same audio seeds the key generator: the PSP has no random number
 * hardware a program can use, and the connection to Muse needs
 * unpredictable keys. */
static short room[MIC_CHUNK] __attribute__((aligned(64)));

static void listen_to_room(void)
{
    static int chunks, loudest;
    if (sceAudioInputBlocking(MIC_CHUNK, MIC_RATE, room) < 0) {
        SDL_Delay(10);
        return;
    }
    if (chunks < 64) {
        muse_psp_add_entropy(room, sizeof(room));
    }
    /* The level each second for the first half minute, to show in the log
     * when the microphone comes alive. */
    if (chunks < 30 * MIC_RATE / MIC_CHUNK) {
        for (int i = 0; i < MIC_CHUNK; i++) {
            loudest = abs(room[i]) > loudest ? abs(room[i]) : loudest;
        }
        if (++chunks % (MIC_RATE / MIC_CHUNK) == 0) {
            SDL_Log("room level %d at %d s", loudest, chunks * MIC_CHUNK / MIC_RATE);
            loudest = 0;
        }
    }
}

/* Two short notes: rising when recording starts, falling when it stops. */
static void play_cue(int channel, short *stereo, int cue)
{
    enum { FRAMES = MIC_CHUNK * 2, NOTES = 2 };
    for (int note = 0; note < NOTES; note++) {
        float pitch = (note == 0) == (cue == CUE_START) ? 880 : 1320;
        for (int i = 0; i < FRAMES; i++) {
            int n = note * FRAMES + i, left = NOTES * FRAMES - n;
            float fade = n < 300 ? n / 300.0f : left < 300 ? left / 300.0f : 1;
            short v = (short)(5000 * fade * sinf(n * 6.2832f * pitch / 44100));
            stereo[i * 2] = stereo[i * 2 + 1] = v;
        }
        sceAudioOutputPannedBlocking(channel, PSP_AUDIO_VOLUME_MAX, PSP_AUDIO_VOLUME_MAX, stereo);
    }
    if (cue == CUE_START) {
        /* The speaker is still sounding the last note. Skip past it so the
         * recording does not open with the cue. */
        for (int i = 0; i < 4; i++) {
            sceAudioInputBlocking(MIC_CHUNK, MIC_RATE, room);
        }
    }
}

static int mic_worker(void *unused)
{
    static short stereo[MIC_CHUNK * 2 * 2];
    int channel = sceAudioChReserve(PSP_AUDIO_NEXT_CHANNEL, MIC_CHUNK * 2, PSP_AUDIO_FORMAT_STEREO);
    SDL_Log("sceAudioInputInit -> 0x%08x", sceAudioInputInit(0, MIC_GAIN, 0));
    for (;;) {
        if (mic.cue) {
            play_cue(channel, stereo, mic.cue);
            mic.cue = CUE_NONE;
        }
        if (mic.state == MIC_RECORDING) {
            if (mic.samples + MIC_CHUNK > MIC_MAX_SAMPLES) {
                mic.state = MIC_RECORDED;
                continue;
            }
            /* Captured into the small buffer the idle listening uses, then
             * copied. On a real PSP the first take captured straight into
             * the big array came back as exact zeros, while the idle buffer
             * was hearing the room a second before and a second after. */
            int result = sceAudioInputBlocking(MIC_CHUNK, MIC_RATE, room);
            memcpy(mic.take + mic.samples, room, sizeof(room));
            if (result < 0) {
                SDL_Log("sceAudioInputBlocking -> 0x%08x", result);
                mic.error = result;
                mic.state = MIC_RECORDED;
                continue;
            }
            int peak = 0;
            for (int i = 0; i < MIC_CHUNK; i++) {
                int v = abs(mic.take[mic.samples + i]);
                peak = v > peak ? v : peak;
            }
            mic.level = peak;
            mic.peak = peak > mic.peak ? peak : mic.peak;
            mic.samples += MIC_CHUNK;
        } else if (mic.state == MIC_PLAYING) {
            /* An answer is still downloading while it plays, so the end moves. */
            const link_speech_t *voice = mic.voice;
            int generation = voice ? voice->generation : 0;
            mic.played = 0;
            while (mic.state == MIC_PLAYING) {
                if (voice && voice->generation != generation) {
                    generation = voice->generation;
                    mic.played = 0;
                }
                int have = voice ? voice->count : mic.samples, complete = !voice || voice->done;
                const short *from = voice ? voice->samples : mic.take;
                if (mic.played >= have && complete) {
                    break;
                }
                if (mic.played + MIC_CHUNK > have && !complete) {
                    SDL_Delay(10);
                    continue;
                }
                /* The speaker runs at 44100 Hz stereo: each sample goes out four times. */
                for (int i = 0; i < MIC_CHUNK; i++) {
                    short v = mic.played + i < have ? from[mic.played + i] : 0;
                    stereo[i * 4] = stereo[i * 4 + 1] = stereo[i * 4 + 2] = stereo[i * 4 + 3] = v;
                }
                sceAudioOutputPannedBlocking(channel, PSP_AUDIO_VOLUME_MAX, PSP_AUDIO_VOLUME_MAX, stereo);
                mic.played += MIC_CHUNK;
            }
            if (mic.state == MIC_PLAYING) {
                mic.state = MIC_DONE;
            }
        } else {
            listen_to_room();
        }
    }
    return 0;
}

static void clear(void)
{
    /* The clip was shot on this colour, so the cut-out edges disappear into it. */
    SDL_SetRenderDrawColor(renderer, CLIP_BG_R, CLIP_BG_G, CLIP_BG_B, 255);
    SDL_RenderClear(renderer);
}

static int load_sheets(void)
{
    char path[32];
    for (int s = 0; s < CLIP_SHEETS; s++) {
        /* Decoding takes about five seconds on a PSP, so show how far along it is. */
        SDL_Rect track = { 140, 133, 200, 6 };
        SDL_Rect fill = { 140, 133, 200 * s / CLIP_SHEETS, 6 };
        clear();
        SDL_SetRenderDrawColor(renderer, 224, 220, 230, 255);
        SDL_RenderFillRect(renderer, &track);
        SDL_SetRenderDrawColor(renderer, 150, 120, 235, 255);
        SDL_RenderFillRect(renderer, &fill);
        SDL_RenderPresent(renderer);

        snprintf(path, sizeof(path), "clip/%02d.png", s);
        /* Held at 4 bits a channel, half the memory. The sheets are already
         * dithered to those 16 levels, so nothing is lost in the conversion. */
        SDL_Surface *decoded = IMG_Load(path);
        SDL_Surface *packed = decoded ? SDL_ConvertSurfaceFormat(decoded, SDL_PIXELFORMAT_ABGR4444, 0) : NULL;
        sheets[s] = packed ? SDL_CreateTextureFromSurface(renderer, packed) : NULL;
        SDL_FreeSurface(decoded);
        SDL_FreeSurface(packed);
        if (!sheets[s]) {
            /* Out of memory part way is survivable: the gesture is cut short
             * to the frames that did load. */
            SDL_Log("cannot load %s: %s", path, SDL_GetError());
            break;
        }
        SDL_SetTextureBlendMode(sheets[s], SDL_BLENDMODE_BLEND);
        frames_loaded = (s + 1) * PER_SHEET < CLIP_FRAMES ? (s + 1) * PER_SHEET : CLIP_FRAMES;
    }
    Uint32 format = 0;
    if (sheets[0]) {
        SDL_QueryTexture(sheets[0], &format, NULL, NULL, NULL);
    }
    SDL_Log("%d of %d frames loaded, held as %s", frames_loaded, CLIP_FRAMES, SDL_GetPixelFormatName(format));
    return frames_loaded > 0;
}

/* A soft round light, added over the orb: red while listening, purple
 * while Muse is thinking. White here, tinted when drawn. */
static SDL_Texture *make_glow(void)
{
    enum { SIZE = 64 };
    SDL_Surface *surface = SDL_CreateRGBSurfaceWithFormat(0, SIZE, SIZE, 32, SDL_PIXELFORMAT_RGBA32);
    Uint8 *pixels = surface->pixels;
    for (int y = 0; y < SIZE; y++) {
        for (int x = 0; x < SIZE; x++) {
            float d = sqrtf((x - 31.5f) * (x - 31.5f) + (y - 31.5f) * (y - 31.5f)) / 32.0f;
            float a = d >= 1 ? 0 : (1 - d) * (1 - d);
            Uint8 *p = pixels + y * surface->pitch + x * 4;
            p[0] = 255; p[1] = 255; p[2] = 255; p[3] = (Uint8)(a * 255);
        }
    }
    SDL_Texture *texture = SDL_CreateTextureFromSurface(renderer, surface);
    SDL_FreeSurface(surface);
    SDL_SetTextureBlendMode(texture, SDL_BLENDMODE_ADD);
    return texture;
}

static void set_label(label_t *label, TTF_Font *font, const char *text, SDL_Color color, int wrap)
{
    if (label->texture) {
        SDL_DestroyTexture(label->texture);
        label->texture = NULL;
    }
    if (!text || !text[0]) {
        return;
    }
    SDL_Surface *surface = wrap ? TTF_RenderUTF8_Blended_Wrapped(font, text, color, wrap)
                                : TTF_RenderUTF8_Blended(font, text, color);
    if (!surface) {
        return;
    }
    label->texture = SDL_CreateTextureFromSurface(renderer, surface);
    label->w = surface->w;
    label->h = surface->h;
    SDL_FreeSurface(surface);
}

static void draw_label(const label_t *label, int x, int y)
{
    if (label->texture) {
        SDL_Rect dst = { x, y, label->w, label->h };
        SDL_RenderCopy(renderer, label->texture, NULL, &dst);
    }
}

static int text_width(TTF_Font *font, const char *text, int from, int to)
{
    char piece[256];
    int length = to - from < (int)sizeof(piece) - 1 ? to - from : (int)sizeof(piece) - 1, w = 0;
    memcpy(piece, text + from, (size_t)length);
    piece[length] = 0;
    TTF_SizeUTF8(font, piece, &w, NULL);
    return w;
}

static void set_caption(caption_t *c, TTF_Font *font, const char *text)
{
    if (c->texture) {
        SDL_DestroyTexture(c->texture);
    }
    free(c->text);
    memset(c, 0, sizeof(*c));
    if (!text || !text[0]) {
        return;
    }
    c->font = font;
    c->text = malloc(strlen(text) + 1);
    strcpy(c->text, text);
    c->line_h = TTF_FontLineSkip(font);
    /* One texture holds every line, and a PSP texture stops at 512 pixels. */
    int most = 512 / c->line_h < CAPTION_LINES ? 512 / c->line_h : CAPTION_LINES;

    int at = 0, length = (int)strlen(text);
    while (at < length && c->lines < most) {
        int end = at, next = at;
        for (;;) {
            int word = next;
            while (word < length && text[word] != ' ' && text[word] != '\n') {
                word++;
            }
            if (end > at && text_width(font, text, at, word) > PANEL_W) {
                break;
            }
            end = word;
            next = word < length ? word + 1 : word;
            if (word >= length || text[word] == '\n') {
                break;
            }
        }
        c->start[c->lines] = at;
        c->end[c->lines] = end;
        c->lines++;
        at = next > end ? next : end;
    }

    static const SDL_Color ink = { 46, 40, 62, 255 };
    SDL_Surface *all = SDL_CreateRGBSurfaceWithFormat(0, PANEL_W, c->lines * c->line_h, 32,
                                                      SDL_PIXELFORMAT_RGBA32);
    SDL_FillRect(all, NULL, SDL_MapRGBA(all->format, ink.r, ink.g, ink.b, 0));
    for (int i = 0; i < c->lines; i++) {
        char line[256];
        int n = c->end[i] - c->start[i] < (int)sizeof(line) - 1 ? c->end[i] - c->start[i]
                                                                : (int)sizeof(line) - 1;
        memcpy(line, text + c->start[i], (size_t)n);
        line[n] = 0;
        SDL_Surface *row = n ? TTF_RenderUTF8_Blended(font, line, ink) : NULL;
        if (row) {
            SDL_Rect to = { 0, i * c->line_h, row->w, row->h };
            SDL_SetSurfaceBlendMode(row, SDL_BLENDMODE_NONE);
            SDL_BlitSurface(row, NULL, all, &to);
            SDL_FreeSurface(row);
        }
    }
    c->texture = SDL_CreateTextureFromSurface(renderer, all);
    SDL_FreeSurface(all);
}

/* How far into the caption's text the voice has got, or -1 when it is not
 * speaking. Before the download ends the length is unknown, so it assumes a
 * usual speaking pace, then switches to the real proportion. */
static int spoken_chars(const caption_t *c)
{
    const link_speech_t *voice = mic.voice;
    if (mic.state != MIC_PLAYING || !voice || !c->text) {
        return -1;
    }
    int count = voice->count, chars = voice->chars, played = mic.played;
    int said = voice->done && count > 0 ? (int)((long long)chars * played / count)
                                        : played * 15 / MIC_RATE;
    int at = voice->first_char + (said < chars ? said : chars), length = (int)strlen(c->text);
    at = at < length ? at : length;
    while (at < length && c->text[at] != ' ' && c->text[at] != '\n') {
        at++;
    }
    return at;
}

static void draw_caption(caption_t *c, int x)
{
    if (!c->texture) {
        return;
    }
    int spoken = spoken_chars(c), current = 0;
    int visible = (SCREEN_H - CAPTION_TOP - 10) / c->line_h;
    visible = visible < c->lines ? visible : c->lines;
    for (int i = 0; i < c->lines; i++) {
        if (spoken >= c->start[i]) {
            current = i;
        }
    }
    /* Keeps the line being spoken one above the bottom of the panel. */
    int first = current - (visible - 2);
    first = first > c->lines - visible ? c->lines - visible : first;
    first = first < 0 ? 0 : first;
    int y = (SCREEN_H - visible * c->line_h) / 2;
    y = y < CAPTION_TOP ? CAPTION_TOP : y;
    SDL_Rect src = { 0, first * c->line_h, PANEL_W, visible * c->line_h };
    SDL_Rect dst = { x, y, PANEL_W, visible * c->line_h };
    SDL_RenderCopy(renderer, c->texture, &src, &dst);
}

/* Where the caption's first visible row sits, for the title above it. */
static int caption_top(const caption_t *c)
{
    int visible = (SCREEN_H - CAPTION_TOP - 10) / (c->line_h ? c->line_h : 1);
    visible = visible < c->lines ? visible : c->lines;
    int y = (SCREEN_H - visible * c->line_h) / 2;
    return y < CAPTION_TOP ? CAPTION_TOP : y;
}

static void show_message(app_t *app, const char *title, const char *text, Uint32 now)
{
    static const SDL_Color muted = { 138, 126, 160, 255 };
    size_t length = strlen(text);
    set_label(&app->title, font_title, title, muted, 0);
    set_caption(&app->body, font_body[length < 70 ? 0 : length < 150 ? 1 : 2], text);
    app->has_message = 1;
    app->message_at = now;
    app->gesture_at = now;      /* it reacts to the message with its gesture */
}

static void set_link(app_t *app, link_state_t link)
{
    static const SDL_Color muted = { 138, 126, 160, 255 };
    app->link = link;
    set_label(&app->status, font_small, LINKS[link].label, muted, 0);
}

static void draw_avatar(app_t *app, Uint32 now)
{
    /* Rest on the first frame most of the time, and play the clip's gesture now and then. */
    int frame = 0;
    if (app->gesture_at) {
        frame = (int)((now - app->gesture_at) * CLIP_FPS / 1000);
        if (frame >= frames_loaded) {
            frame = 0;
            app->gesture_at = 0;
            app->next_gesture = now + 2500 + rand() % 4000;
        }
    } else if (now >= app->next_gesture) {
        app->gesture_at = now;
    }

    float t = now / 1000.0f;
    float target = app->has_message && now >= app->dance_until ? 14 : (SCREEN_W - CLIP_CELL_W) / 2.0f;
    app->avatar_x += (target - app->avatar_x) * 0.14f;

    float x = app->avatar_x, lift = 0, breathe = 1 + 0.012f * sinf(t * 2.2f);
    if (now < app->dance_until) {
        x += 18 * sinf(t * 6.283f);                 /* one sway a second */
        lift = 14 * fabsf(sinf(t * 6.283f));        /* two hops a second */
    } else if (app->message_at && now - app->message_at < 600) {
        float p = (now - app->message_at) / 600.0f; /* one settling bounce */
        lift = 12 * fabsf(sinf(p * 9.42f)) * (1 - p);
    }
    if (app->error) {
        x += 3 * sinf(t * 40);
    }

    SDL_Texture *sheet = sheets[frame / PER_SHEET];
    int slot = frame % PER_SHEET;
    SDL_Rect src = { (slot % CLIP_COLUMNS) * CLIP_CELL_W, (slot / CLIP_COLUMNS) * CLIP_CELL_H,
                     CLIP_CELL_W, CLIP_CELL_H };
    int h = (int)(CLIP_CELL_H * breathe + 0.5f);
    SDL_Rect dst = { (int)x, FEET_Y - h - (int)lift, CLIP_CELL_W, h };
    SDL_SetTextureColorMod(sheet, 255, app->error ? 170 : 255, app->error ? 170 : 255);
    SDL_RenderCopy(renderer, sheet, &src, &dst);

    int listening = mic.state == MIC_RECORDING;
    if (app->busy || listening) {
        SDL_SetTextureColorMod(glow, listening ? 255 : 190, listening ? 70 : 150, listening ? 95 : 255);
        int size = 70 + (int)(22 * sinf(t * 5));
        SDL_Rect orb = { dst.x + CLIP_CELL_W / 2 - size / 2, dst.y + h / 2 - size / 2 + 6, size, size };
        SDL_SetTextureAlphaMod(glow, 150 + (int)(90 * sinf(t * 5)));
        SDL_RenderCopy(renderer, glow, NULL, &orb);
    }
}

static void draw_meter(const label_t *label, int filled, SDL_Color color)
{
    SDL_Rect track = { PANEL_X, 140, PANEL_W, 10 }, fill = { PANEL_X, 140, filled, 10 };
    draw_label(label, PANEL_X, 104);
    SDL_SetRenderDrawColor(renderer, 224, 220, 230, 255);
    SDL_RenderFillRect(renderer, &track);
    SDL_SetRenderDrawColor(renderer, color.r, color.g, color.b, 255);
    SDL_RenderFillRect(renderer, &fill);
}

/* The voice list, opened with Select: a row per voice, the cursor's row
 * filled, and a dot beside the voice in use. */
static void draw_picker(app_t *app)
{
    enum { ROW_H = 26, TOP = 58 };
    draw_label(&app->picker_title, PANEL_X, 26);
    for (int i = 0; i < link_voice_count() && i < VOICE_ROWS; i++) {
        int y = TOP + i * ROW_H;
        if (i == app->picking) {
            SDL_Rect row = { PANEL_X - 8, y - 3, PANEL_W + 8, ROW_H - 2 };
            SDL_SetRenderDrawColor(renderer, 226, 218, 244, 255);
            SDL_RenderFillRect(renderer, &row);
        }
        if (i == link_voice()) {
            SDL_Rect dot = { PANEL_X, y + 7, 8, 8 };
            SDL_SetRenderDrawColor(renderer, 150, 120, 235, 255);
            SDL_RenderFillRect(renderer, &dot);
        }
        draw_label(&app->voices[i], PANEL_X + 18, y);
    }
    draw_label(&app->picker_hint, PANEL_X, SCREEN_H - 24);
}

static void draw(app_t *app, Uint32 now)
{
    clear();
    draw_avatar(app, now);

    int dancing = now < app->dance_until;
    if (app->picking >= 0) {
        draw_picker(app);
    } else if (mic.state == MIC_RECORDING) {
        /* Bars rising and falling like a voice memo. They run on a loop
         * and do not depend on what the microphone hears. */
        enum { BARS = 7, BAR_W = 8, BAR_GAP = 8, BAR_MAX = 40 };
        static const float speed[BARS] = { 7.1f, 9.4f, 6.2f, 10.3f, 8.0f, 6.7f, 9.0f };
        draw_label(&app->listening, PANEL_X, 104);
        SDL_SetRenderDrawColor(renderer, 235, 90, 110, 255);
        for (int i = 0; i < BARS; i++) {
            float wave = 0.5f + 0.5f * sinf(now / 1000.0f * speed[i] + i * 1.7f);
            int h = 8 + (int)((BAR_MAX - 8) * wave * wave);
            SDL_Rect bar = { PANEL_X + i * (BAR_W + BAR_GAP), 164 - h / 2, BAR_W, h };
            SDL_RenderFillRect(renderer, &bar);
        }
    } else if (mic.state == MIC_PLAYING && !mic.voice) {
        draw_meter(&app->playing,
                   mic.samples ? (int)((long long)PANEL_W * mic.played / mic.samples) : 0,
                   (SDL_Color){ 150, 120, 235, 255 });
    } else if (app->has_message && !dancing) {
        draw_label(&app->title, PANEL_X, caption_top(&app->body) - 26);
        draw_caption(&app->body, PANEL_X);
    } else {
        /* Top left, clear of the character's head. */
        SDL_Rect dot = { 16, 18, 8, 8 };
        SDL_Color c = LINKS[app->link].dot;
        SDL_SetRenderDrawColor(renderer, c.r, c.g, c.b, 255);
        SDL_RenderFillRect(renderer, &dot);
        draw_label(&app->status, 30, 12);
    }
    SDL_RenderPresent(renderer);
}

static int send_take(void)
{
    int length = 0;
    unsigned char *wav = take_as_wav(&length);
    int sent = wav && link_ask_voice(wav, length);
    free(wav);
    return sent;
}

int main(int argc, char *argv[])
{
    /* Full speed: the encryption for Muse and for speech is heavy work here. */
    scePowerSetClockFrequency(333, 333, 166);
    if (SDL_Init(SDL_INIT_VIDEO | SDL_INIT_JOYSTICK) != 0 || TTF_Init() != 0) {
        return 1;
    }
    SDL_SetHint(SDL_HINT_RENDER_SCALE_QUALITY, "1");
    SDL_Window *window = SDL_CreateWindow("Muse", SDL_WINDOWPOS_UNDEFINED, SDL_WINDOWPOS_UNDEFINED,
                                          SCREEN_W, SCREEN_H, 0);
    renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    SDL_JoystickOpen(0);
    IMG_Init(IMG_INIT_PNG);

    static const int body_sizes[3] = { 22, 18, 15 };
    font_title = TTF_OpenFont("fonts/Nunito-ExtraBold.ttf", 15);
    font_small = TTF_OpenFont("fonts/Nunito-ExtraBold.ttf", 13);
    for (int i = 0; i < 3; i++) {
        font_body[i] = TTF_OpenFont("fonts/Nunito-Medium.ttf", body_sizes[i]);
    }
    SDL_Log("memory free at start: %d KB", (int)(sceKernelTotalFreeMemSize() / 1024));
    /* The connection and the voices start before the avatar's frames load.
     * They take the memory they need first, and Muse is connecting during
     * the seconds the frames take. */
    SDL_CreateThread(mic_worker, "mic", NULL);
    link_start();
    SDL_Log("memory free once the voices are ready: %d KB", (int)(sceKernelTotalFreeMemSize() / 1024));
    Uint32 load_started = SDL_GetTicks();
    if (!renderer || !font_title || !font_small || !font_body[2] || !load_sheets()) {
        SDL_Log("startup failed: %s", SDL_GetError());
        SDL_Quit();
        return 1;
    }
    glow = make_glow();
    SDL_Log("loaded %d sheets in %u ms, %d KB of memory left", CLIP_SHEETS,
            (unsigned)(SDL_GetTicks() - load_started), (int)(sceKernelTotalFreeMemSize() / 1024));

    app_t app = { 0 };
    app.avatar_x = (SCREEN_W - CLIP_CELL_W) / 2.0f;
    app.next_gesture = SDL_GetTicks() + 1500;
    set_link(&app, LINK_NO_WIFI);
    app.picking = -1;
    set_label(&app.picker_title, font_title, "Voice", (SDL_Color){ 138, 126, 160, 255 }, 0);
    set_label(&app.picker_hint, font_small, "X choose and hear it      O close", (SDL_Color){ 138, 126, 160, 255 }, 0);
    for (int i = 0; i < link_voice_count() && i < VOICE_ROWS; i++) {
        set_label(&app.voices[i], font_body[1], link_voice_label(i), (SDL_Color){ 46, 40, 62, 255 }, 0);
    }
    set_label(&app.listening, font_body[0], "Listening", (SDL_Color){ 46, 40, 62, 255 }, 0);
    set_label(&app.playing, font_body[0], "Playing it back", (SDL_Color){ 46, 40, 62, 255 }, 0);

    /* A demo.txt beside the app opens with that text as a message, which is
     * how the message screen gets checked in the emulator, where nobody can
     * press a button. */
    FILE *demo = fopen("demo.txt", "r");
    if (demo) {
        char text[400] = "";
        size_t n = fread(text, 1, sizeof(text) - 1, demo);
        fclose(demo);
        while (n && (text[n - 1] == '\n' || text[n - 1] == '\r')) {
            text[--n] = 0;
        }
        show_message(&app, "Muse", text, SDL_GetTicks());
    }

    int sample = 0, running = 1;
    while (running) {
        Uint32 now = SDL_GetTicks();
        int online = link_state() == LINK_UP;
        if (link_state() != app.link) {
            set_link(&app, link_state());
            if (!online) {
                app.busy = 0;
            }
        }

        SDL_Event event;
        while (SDL_PollEvent(&event)) {
            if (event.type == SDL_QUIT) {
                running = 0;
            } else if (event.type == SDL_JOYBUTTONDOWN) {
                int button = event.jbutton.button;
                if (button == BTN_START) {
                    running = 0;
                } else if (app.picking >= 0) {
                    int count = link_voice_count() < VOICE_ROWS ? link_voice_count() : VOICE_ROWS;
                    if (button == BTN_UP || button == BTN_DOWN) {
                        app.picking = (app.picking + (button == BTN_DOWN ? 1 : count - 1)) % count;
                    } else if (button == BTN_CROSS) {
                        link_set_voice(app.picking);
                    } else if (button == BTN_CIRCLE || button == BTN_SELECT) {
                        app.picking = -1;
                        app.has_message = 0;
                    }
                } else if (button == BTN_SELECT && mic.state != MIC_RECORDING) {
                    app.picking = link_voice();
                    app.has_message = 1;    /* slides the character aside for the list */
                } else if (button == BTN_R) {
                    mic.samples = mic.level = mic.peak = mic.error = 0;
                    app.has_message = 1;    /* slides the character aside for the meter */
                    set_label(&app.title, font_title, NULL, (SDL_Color){ 0 }, 0);
                    set_caption(&app.body, NULL, NULL);
                    mic.cue = CUE_START;
                    mic.state = MIC_RECORDING;
                } else if (button == BTN_CIRCLE) {
                    app.has_message = 0;
                } else if (button == BTN_CROSS) {
                    show_message(&app, SAMPLES[sample].title, SAMPLES[sample].text, now);
                    sample = (sample + 1) % (int)(sizeof(SAMPLES) / sizeof(SAMPLES[0]));
                } else if (button == BTN_TRIANGLE) {
                    app.dance_until = now + DANCE_MS;
                }
            } else if (event.type == SDL_JOYBUTTONUP && event.jbutton.button == BTN_R) {
                if (mic.state == MIC_RECORDING) {
                    mic.cue = CUE_STOP;
                }
                if (mic.state == MIC_RECORDING) {
                    mic.state = MIC_RECORDED;
                }
            }
        }

        if (mic.state == MIC_RECORDED) {
            SDL_Log("recorded %d samples, peak %d", mic.samples, mic.peak);
            if (mic.error) {
                mic.state = MIC_DONE;
            } else if (online && send_take()) {
                app.has_message = 0;
                app.busy = 1;
                mic.state = MIC_IDLE;
            } else if (online) {
                /* Muse takes one question at a time, and the last answer is
                 * confirmed a moment after its voice starts. */
                show_message(&app, "Muse", "One moment, I am still finishing my last answer.", now);
                mic.state = MIC_IDLE;
            } else {
                mic.voice = NULL;
                mic.state = MIC_PLAYING;
            }
        }
        if (mic.state == MIC_DONE) {
            if (!mic.voice || mic.error) {
                char report[160];
                int percent = mic.peak * 100 / 32767;
                if (mic.error) {
                    snprintf(report, sizeof(report), "The microphone returned error %08X.", (unsigned)mic.error);
                } else if (percent < 2) {
                    snprintf(report, sizeof(report), "I recorded %.1f seconds of silence. The microphone "
                             "may need more gain.", mic.samples / (float)MIC_RATE);
                } else {
                    snprintf(report, sizeof(report), "I heard you for %.1f seconds. Loudest point %d%%.",
                             mic.samples / (float)MIC_RATE, percent);
                }
                show_message(&app, "Microphone test", report, now);
            }
            mic.state = MIC_IDLE;
        }

        char type, *payload;
        int length;
        while (link_next(&type, &payload, &length)) {
            if (type == 'M') {
                char *text = strchr(payload, '\n');
                if (text) {
                    *text++ = 0;
                    show_message(&app, payload, text, now);
                }
            } else if (type == 'C') {
                app.has_message = 0;
            } else if (type == 'D') {
                app.dance_until = now + (Uint32)atoi(payload) * 1000;
            } else if (type == 'T') {
                app.busy = payload[0] == '1';
            } else if (type == 'S' && mic.state != MIC_RECORDING && mic.state != MIC_RECORDED) {
                if (mic.state == MIC_PLAYING && !mic.voice) {
                    /* Let the audio thread finish its chunk of the take and let go. */
                    mic.state = MIC_IDLE;
                    SDL_Delay(80);
                }
                mic.voice = link_speech();
                mic.state = MIC_PLAYING;
            }
            free(payload);
        }
        draw(&app, now);
    }
    SDL_Quit();
    return 0;
}
