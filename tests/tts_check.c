/* The on-device voice, built for this computer: speak a sentence, report how long the
 * audio is, how loud, and how long it took to make.
 *
 *   tts_check <voice data folder> <out.pcm> "text"
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#include "native_tts.h"

static FILE *out;
static long total;
static int peak;

static int sink(void *user, const short *samples, int count)
{
    (void)user;
    for (int i = 0; i < count; i++) {
        peak = abs(samples[i]) > peak ? abs(samples[i]) : peak;
    }
    total += count;
    return fwrite(samples, sizeof(short), (size_t)count, out) != (size_t)count;
}

int main(int argc, char **argv)
{
    if (argc != 4 || !native_tts_start(argv[1])) {
        fprintf(stderr, "could not start the voice\n");
        return 1;
    }
    out = fopen(argv[2], "wb");
    clock_t began = clock();
    int spoke = native_tts_speak(argv[3], sink, NULL);
    double took = (double)(clock() - began) / CLOCKS_PER_SEC;
    fclose(out);
    printf("spoke=%d audio=%.2f s peak=%d made in %.3f s\n", spoke, (double)total / NATIVE_TTS_RATE, peak, took);
    return spoke && total > NATIVE_TTS_RATE / 2 && peak > 2000 ? 0 : 1;
}
