#include "native_tts.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "picoapi.h"
#include "picodefs.h"

#define ENGINE_MEMORY 2500000 /* what the engine's authors give it for one English voice */
#define VOICE "PSP"

static pico_System pico;
static pico_Engine engine;

static int load(const char *folder, const char *file)
{
    char path[160], name[256]; /* the engine writes up to 200 bytes of name */
    pico_Resource resource = NULL;
    snprintf(path, sizeof(path), "%s/%s", folder, file);
    return pico_loadResource(pico, (const pico_Char *)path, &resource) == PICO_OK
           && pico_getResourceName(pico, resource, name) == PICO_OK
           && pico_addResourceToVoiceDefinition(pico, (const pico_Char *)VOICE, (const pico_Char *)name) == PICO_OK;
}

int native_tts_start(const char *folder)
{
    void *memory = malloc(ENGINE_MEMORY);
    if (!memory || pico_initialize(memory, ENGINE_MEMORY, &pico) != PICO_OK
        || pico_createVoiceDefinition(pico, (const pico_Char *)VOICE) != PICO_OK
        || !load(folder, "en-US_ta.bin") || !load(folder, "en-US_lh0_sg.bin")
        || pico_newEngine(pico, (const pico_Char *)VOICE, &engine) != PICO_OK) {
        engine = NULL;
        free(memory);
        return 0;
    }
    return 1;
}

int native_tts_speak(const char *text, int (*sink)(void *user, const short *samples, int count), void *user)
{
    if (!engine) {
        return 0;
    }
    /* The engine reads plain text. Markdown marks and anything outside
     * ASCII, emoji above all, would be spelled out or stall it. */
    size_t length = strlen(text);
    char *plain = malloc(length + 2);
    size_t n = 0;
    for (size_t i = 0; i < length; i++) {
        unsigned char c = (unsigned char)text[i];
        if (c >= 0x80 || c == '*' || c == '_' || c == '#' || c == '`') {
            continue;
        }
        plain[n++] = c == '\n' ? ' ' : (char)c;
    }
    plain[n++] = 0; /* the terminator is sent too: it tells the engine the text is complete */

    int spoke = 0, stopped = 0;
    for (size_t sent = 0; sent < n && !stopped;) {
        pico_Int16 took = 0;
        size_t rest = n - sent;
        if (pico_putTextUtf8(engine, (const pico_Char *)plain + sent, (pico_Int16)(rest > 256 ? 256 : rest), &took)
            != PICO_OK) {
            break;
        }
        sent += (size_t)took;
        int status;
        do {
            short samples[256];
            pico_Int16 bytes = 0, kind = 0;
            status = pico_getData(engine, samples, sizeof(samples), &bytes, &kind);
            if (bytes > 0) {
                spoke = 1;
                stopped = sink(user, samples, bytes / 2);
            }
        } while (status == PICO_STEP_BUSY && !stopped);
        if (status != PICO_STEP_IDLE && status != PICO_STEP_BUSY) {
            break;
        }
    }
    if (stopped) {
        pico_resetEngine(engine, PICO_RESET_SOFT);
    }
    free(plain);
    return spoke;
}
