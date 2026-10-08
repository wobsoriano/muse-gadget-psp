/* Renders the Muse Gadget SDK's own avatar into a file the PSP app plays.
 *
 * The SDK's renderer is compiled in from a local clone, see tools/avatar.sh.
 * Its artwork is not under the SDK's Apache licence, so neither it nor what
 * this writes belongs in a repository.
 *
 * Output, little endian:
 *   "MUSEAV1\0", u16 width, height, colours, frames per second, clips
 *   per clip:  u16 frames, u16 loops (1 or 0)
 *   per frame: u16 colour[colours], u16 edge colour[colours], u8 pixel[width * height]
 * Colours are PSP 5650: red in the low bits.
 */
#include "muse_pixel.c"

#include <stdio.h>

#define FPS 20

typedef struct {
    muse_mode_t mode;
    float happy;
    float seconds;
    int loops;
} clip_t;

/* The order is the app's: src/avatar.rs names clips by these positions. */
static const clip_t CLIPS[] = {
    { MUSE_MODE_BOOT, 0, 4, 1 },      { MUSE_MODE_IDLE, 0, 4, 1 },  { MUSE_MODE_LISTENING, 0, 4, 1 },
    { MUSE_MODE_THINKING, 0, 4, 1 },  { MUSE_MODE_SPEAKING, 0, 4, 1 }, { MUSE_MODE_ERROR, 0, 4, 1 },
    { MUSE_MODE_OFF, 0, 2.5f, 0 },    { MUSE_MODE_IDLE, 1, 4, 1 },
};

static void put16(FILE *out, unsigned value)
{
    fputc(value & 0xFF, out);
    fputc(value >> 8 & 0xFF, out);
}

/* The SDK packs red high, the PSP wants it low. */
static unsigned psp_colour(uint16_t c)
{
    return (unsigned)((c & 0x1F) << 11 | (c & 0x07E0) | c >> 11);
}

/* A stand-in for a voice, so the speaking clip moves its mouth. */
static float voice(float t)
{
    float level = 0.15f + 0.55f * fabsf(sinf(6.3f * t)) * (0.6f + 0.4f * sinf(1.7f * t));
    return level < 0 ? 0 : level > 1 ? 1 : level;
}

int main(int argc, char **argv)
{
    FILE *out = argc == 2 ? fopen(argv[1], "wb") : NULL;
    if (!out) {
        fprintf(stderr, "usage: render_avatar OUTPUT\n");
        return 1;
    }
    int clips = (int)(sizeof(CLIPS) / sizeof(CLIPS[0]));
    fwrite("MUSEAV1", 1, 8, out);
    put16(out, W);
    put16(out, H);
    put16(out, C_COUNT);
    put16(out, FPS);
    put16(out, (unsigned)clips);
    for (int i = 0; i < clips; i++) {
        put16(out, (unsigned)(CLIPS[i].seconds * FPS));
        put16(out, (unsigned)CLIPS[i].loops);
    }

    float t = 1;
    for (int i = 0; i < clips; i++) {
        const clip_t *clip = &CLIPS[i];
        /* A looping clip starts once its colours have settled. One that plays
         * through starts at the moment the mode begins. */
        int settle = clip->loops ? (int)(1.5f * FPS) : 0;
        int frames = (int)(clip->seconds * FPS);
        for (int frame = -settle; frame < frames; frame++) {
            t += 1.0f / FPS;
            muse_pose_t pose = {
                .mode = clip->mode,
                .t = t,
                .mode_t = (float)(frame + settle) / FPS,
                .level = clip->mode == MUSE_MODE_SPEAKING ? voice(t) : 0,
                .happy = clip->happy,
            };
            muse_pixel_render(&pose);
            if (frame < 0) {
                continue;
            }
            for (int c = 0; c < C_COUNT; c++) {
                put16(out, psp_colour(s_pal[c]));
            }
            for (int c = 0; c < C_COUNT; c++) {
                put16(out, psp_colour(s_pal_dim[c]));
            }
            fwrite(s_fb, 1, sizeof(s_fb), out);
        }
    }
    return fclose(out) == 0 ? 0 : 1;
}
