/* The PSP's own connection to Muse.
 *
 * A thread joins Wi-Fi with the first saved connection, then runs the Muse
 * gadget client (../muse) with the pairing kept in state/ on the Memory
 * Stick. What Muse asks for and what it answers arrive as small frames for
 * the main loop:
 *
 *   'M' "title\ntext"   show a message      'C' clear the message
 *   'D' seconds         dance               'T' "0" the turn has ended
 *   'S' the answer has started arriving as speech, see link_speech()
 */
#ifndef LINK_H
#define LINK_H

#define LINK_AUDIO_RATE 22050

typedef enum {
    LINK_UNPAIRED, LINK_NO_WIFI, LINK_JOINING, LINK_CONNECTING, LINK_UP, LINK_STATES
} link_state_t;

/* The spoken answer. The speech thread fills it while it downloads and the
 * speaker plays from the front, so count grows under the reader. */
typedef struct {
    const short *samples;       /* mono, LINK_AUDIO_RATE */
    volatile int count;         /* samples written so far */
    volatile int done;          /* no more will arrive */
    volatile int generation;    /* goes up each time a new answer starts over at 0 */
    volatile int first_char;    /* the part of the shown text being spoken */
    volatile int chars;
} link_speech_t;

const link_speech_t *link_speech(void);

/* The voices answers can be spoken in. Choosing one saves the choice and
 * speaks a short sample in it. */
int link_voice_count(void);
const char *link_voice_label(int voice);
int link_voice(void);
void link_set_voice(int voice);

void link_start(void);
link_state_t link_state(void);

/* Next frame, or 0 if none. The caller frees *payload, which is NUL terminated. */
int link_next(char *type, char **payload, int *length);

/* Send a recording (a complete WAV file) to Muse as a voice note. The answer
 * arrives as an 'M' frame. Returns 0 when Muse is not reachable or is still
 * answering the last one. */
int link_ask_voice(const void *wav, int length);

#endif
