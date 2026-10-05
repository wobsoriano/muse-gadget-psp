/* Speech made on the PSP itself, with the Pico engine (third_party/pico).
 *
 * It sounds synthetic next to a cloud voice, but needs no key, no network
 * and no waiting, so it is both a voice the user can choose and what the
 * app falls back to when the cloud voice cannot be reached.
 */
#ifndef NATIVE_TTS_H
#define NATIVE_TTS_H

#define NATIVE_TTS_RATE 16000

/* Loads the engine and the voice data from `folder`. Returns 0 when the
 * voice is unavailable, after which native_tts_speak does nothing. */
int native_tts_start(const char *folder);

/* Speaks `text`, handing 16-bit mono samples at NATIVE_TTS_RATE to `sink` as
 * they are made. A nonzero return from sink stops early. Returns 1 when
 * anything was spoken. */
int native_tts_speak(const char *text, int (*sink)(void *user, const short *samples, int count), void *user);

#endif
