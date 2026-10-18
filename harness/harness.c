/*
 * harness.c - drives the LibDTSX C API over a DTS-HD elementary stream (.dts).
 *
 *   harness [options] <stream.dts>
 *     --chunk N             push N bytes at a time (default 4096)
 *     --random-chunks       push random chunks of 1..16384 bytes (fixed seed)
 *     --render out.f32      write a 7.1.4 render, interleaved Float32 in the order
 *                           L R C LFE Ls Rs Lb Rb Tfl Tfr Tbl Tbr, made by SpatialAudioBridge's
 *                           DTS:X rule (beds and fixed channels routed to their speaker; an
 *                           object channel fails the render, the library has none), so `cmp`
 *                           against its `render` example output checks the decode and the fold
 *                           removal
 *     --expect-refusal S    the first pull must return status S; checks that push() and pull()
 *                           keep returning it and that reset() keeps it
 *     --expect-presentation P   every block must carry DTSX_PRESENTATION_* P
 *     --seek                after the full decode, reset and resume at frame boundaries at 25,
 *                           50 and 75% (pushing from the frame's first byte): the input timeline
 *                           must restart at 0 and the output must match the continuous decode
 *
 *   harness --compare a.f32 b.f32 FRAME
 *                           compares two 7.1.4 renders from input frame FRAME on: they must be
 *                           bit-identical.
 *
 * Exit status 0 when every requested check passes. Compile with -ffp-contract=off so the render
 * rounds as the Rust reference does.
 */
#include "dtsx.h"

#include <inttypes.h>
#include <math.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define OUT_CHANNELS 12

static int failures = 0;
#define CHECK(cond, ...)                                                                          \
    do {                                                                                          \
        if (!(cond)) {                                                                            \
            fprintf(stderr, "FAIL: " __VA_ARGS__);                                                \
            fputc('\n', stderr);                                                                  \
            failures++;                                                                           \
        }                                                                                         \
    } while (0)

/* ---- input ----------------------------------------------------------------------------- */

typedef struct {
    uint8_t *data;
    size_t size;
} Input;

static Input load(const char *path) {
    Input in = {0};
    FILE *f = fopen(path, "rb");
    if (!f) {
        perror(path);
        exit(2);
    }
    fseek(f, 0, SEEK_END);
    in.size = (size_t)ftell(f);
    fseek(f, 0, SEEK_SET);
    in.data = malloc(in.size ? in.size : 1);
    if (fread(in.data, 1, in.size, f) != in.size) {
        perror(path);
        exit(2);
    }
    fclose(f);
    return in;
}

static uint32_t bits(const uint8_t *p, size_t bit, int n) {
    uint32_t v = 0;
    for (int i = 0; i < n; i++, bit++) v = v << 1 | ((p[bit / 8] >> (7 - bit % 8)) & 1);
    return v;
}

/* Byte offset of every DTS-HD frame (core + extension substream), for the seek checks. */
static size_t frame_offsets(const Input *in, size_t **out) {
    size_t cap = 1024, n = 0, at = 0;
    *out = malloc(cap * sizeof(size_t));
    while (at + 16 <= in->size) {
        const uint8_t *p = in->data + at;
        if (bits(p, 0, 32) != 0x7FFE8001u) break;
        size_t core = bits(p, 46, 14) + 1, size = core;
        if (at + core + 16 <= in->size && bits(p + core, 0, 32) == 0x64582025u) {
            const uint8_t *x = p + core;
            int wide = (int)bits(x, 42, 1);
            size += bits(x, 43 + (wide ? 12 : 8), wide ? 20 : 16) + 1;
        }
        if (n == cap) *out = realloc(*out, (cap *= 2) * sizeof(size_t));
        (*out)[n++] = at;
        at += size;
    }
    return n;
}

/* ---- SpatialAudioBridge's 7.1.4 DTS:X rule -------------------------------------------- */

/* Output index of each speaker code SpatialAudioBridge's 7.1.4 layout has; -1 otherwise. */
static int output_index(uint8_t speaker) {
    switch (speaker) {
    case DTSX_SPEAKER_L: return 0;
    case DTSX_SPEAKER_R: return 1;
    case DTSX_SPEAKER_C: return 2;
    case DTSX_SPEAKER_LFE: return 3;
    case DTSX_SPEAKER_LS: return 4;
    case DTSX_SPEAKER_RS: return 5;
    case DTSX_SPEAKER_LB: return 6;
    case DTSX_SPEAKER_RB: return 7;
    case DTSX_SPEAKER_TFL: return 8;
    case DTSX_SPEAKER_TFR: return 9;
    case DTSX_SPEAKER_TBL: return 10;
    case DTSX_SPEAKER_TBR: return 11;
    default: return -1;
    }
}

typedef struct {
    FILE *out;
    int routes[DTSX_MAX_CHANNELS]; /* output index of each channel */
    float *buffer;
    bool unsupported;
    uint64_t written; /* frames written, on the input timeline */
} Renderer;

static void render_block(Renderer *r, const DTSXBlock *b) {
    /* Silence for input frames without a block (priming, buffering), as a host fills them. */
    static const float zeros[OUT_CHANNELS * 64];
    while (r->written < b->input_frame_offset) {
        uint64_t n = b->input_frame_offset - r->written;
        if (n > 64) n = 64;
        fwrite(zeros, sizeof(float), OUT_CHANNELS * (size_t)n, r->out);
        r->written += n;
    }
    r->written += b->frame_count;
    for (uint32_t c = 0; c < b->channel_count; c++) {
        r->routes[c] = output_index(b->speakers[c]);
        if (r->routes[c] < 0) r->unsupported = true;
    }
    float *out = r->buffer;
    memset(out, 0, sizeof(float) * OUT_CHANNELS * b->frame_count);
    for (uint32_t s = 0; s < b->frame_count; s++)
        for (uint32_t c = 0; c < b->channel_count; c++)
            if (r->routes[c] >= 0) out[s * OUT_CHANNELS + r->routes[c]] += b->channels[c][s];
    fwrite(out, sizeof(float), OUT_CHANNELS * b->frame_count, r->out);
}

/* ---- decoding ------------------------------------------------------------------------- */

typedef struct {
    uint64_t start;       /* first input access unit kept */
    size_t frames, count; /* frames per block, blocks kept */
    uint32_t channels;
    float *planes;        /* count blocks x channels x frames */
} Window;

typedef struct {
    uint64_t blocks, frames, updates, bed_as_authored, discontinuities, layouts, next_input, first_input;
    uint32_t presentation, channels, rate;
    Window *windows; /* blocks kept for the seek checks */
    int window_count;
    const Window *resumed_from; /* a resumed decode compares itself with this window */
    uint64_t differing, last_differing;
    float worst;
} Summary;

#define WINDOW_BLOCKS 200

/* Keeps the block if a window wants it: copies its planes. */
static void keep(Summary *sum, const DTSXBlock *b) {
    for (int w = 0; w < sum->window_count; w++) {
        Window *win = &sum->windows[w];
        if (b->input_access_unit < win->start || b->input_access_unit >= win->start + WINDOW_BLOCKS) continue;
        if (!win->planes) {
            win->frames = b->frame_count;
            win->channels = b->channel_count;
            win->planes = calloc((size_t)WINDOW_BLOCKS * win->channels * win->frames, sizeof(float));
        }
        size_t i = (size_t)(b->input_access_unit - win->start);
        if (b->frame_count != win->frames || b->channel_count != win->channels) continue;
        for (uint32_t c = 0; c < b->channel_count; c++)
            memcpy(win->planes + (i * win->channels + c) * win->frames, b->channels[c], sizeof(float) * win->frames);
        if (i + 1 > win->count) win->count = i + 1;
    }
}

/* Largest difference between the block and the window's copy of the same input access unit. */
static float block_difference(const Window *win, const DTSXBlock *b, uint64_t access_unit) {
    size_t i = (size_t)(access_unit - win->start);
    if (i >= win->count || b->frame_count != win->frames || b->channel_count != win->channels) return INFINITY;
    float worst = 0;
    for (uint32_t c = 0; c < b->channel_count; c++)
        for (uint32_t s = 0; s < b->frame_count; s++)
            worst = fmaxf(worst, fabsf(b->channels[c][s] - win->planes[(i * win->channels + c) * win->frames + s]));
    return worst;
}

static uint64_t rng_state = 0x9E3779B97F4A7C15ull;
static size_t random_chunk(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return 1 + (size_t)(rng_state % 16384);
}

/* Decodes data[from..] with the given chunking; returns the first non-OK, non-NEED_MORE status. */
static DTSXStatus decode(DTSXDecoder *d, const Input *in, size_t from, size_t chunk, bool random,
                         Summary *sum, Renderer *render, int expect_presentation) {
    size_t at = from;
    while (at < in->size) {
        size_t n = random ? random_chunk() : chunk;
        if (n > in->size - at) n = in->size - at;
        DTSXStatus st = dtsx_decoder_push(d, in->data + at, n, DTSX_NO_PTS);
        if (st != DTSX_OK) return st;
        at += n;
        for (;;) {
            DTSXBlock b;
            st = dtsx_decoder_pull(d, &b);
            if (st == DTSX_NEED_MORE_DATA) break;
            if (st != DTSX_OK) return st;
            CHECK(b.frame_count > 0 && b.frame_count <= DTSX_MAX_BLOCK_FRAMES, "frame_count %u", b.frame_count);
            CHECK(b.metadata != NULL && (b.flags & DTSX_BLOCK_HAS_METADATA), "block without metadata");
            CHECK(b.metadata->element_count == b.channel_count, "metadata element count");
            CHECK(sum->blocks == 0 || b.input_frame_offset == sum->next_input || (b.flags & DTSX_BLOCK_DISCONTINUITY),
                  "input timeline jumps without a discontinuity at %" PRIu64, sum->next_input);
            if (sum->blocks == 0) sum->first_input = b.input_frame_offset;
            sum->next_input = b.input_frame_offset + b.frame_count;
            if (expect_presentation >= 0)
                CHECK(b.presentation == (uint32_t)expect_presentation, "presentation %u", b.presentation);
            for (uint32_t c = 0; c < b.channel_count; c++)
                for (uint32_t s = 0; s < b.frame_count; s++)
                    if (!isfinite(b.channels[c][s])) {
                        CHECK(false, "non-finite sample");
                        c = b.channel_count;
                        break;
                    }
            sum->blocks++;
            sum->frames += b.frame_count;
            sum->updates += b.metadata->update_count;
            sum->bed_as_authored += (b.flags & DTSX_BLOCK_BED_AS_AUTHORED) != 0;
            sum->discontinuities += (b.flags & DTSX_BLOCK_DISCONTINUITY) != 0;
            sum->layouts += (b.flags & DTSX_BLOCK_LAYOUT_CHANGED) != 0;
            sum->presentation = b.presentation;
            sum->channels = b.channel_count;
            sum->rate = b.sample_rate;
            if (sum->window_count) keep(sum, &b);
            if (sum->resumed_from)
                /* A resumed decode keeps the source's time: frames without output (pending in the
                   lossless layer, priming) count too, so the offset is the frame's position. */
                CHECK(b.input_frame_offset == b.input_access_unit * b.access_unit_frames,
                      "resumed block at input frame %" PRIu64 " for access unit %" PRIu64, b.input_frame_offset,
                      b.input_access_unit);
            if (sum->resumed_from) {
                float diff = block_difference(sum->resumed_from, &b, sum->resumed_from->start + b.input_access_unit);
                if (diff > 0 && getenv("HARNESS_VERBOSE")) {
                    const Window *win = sum->resumed_from;
                    size_t i = (size_t)b.input_access_unit;
                    double e_ref = 0, e_diff = 0, e_out = 0;
                    for (uint32_t c = 0; c < b.channel_count; c++)
                        for (uint32_t s = 0; s < b.frame_count; s++) {
                            double r = win->planes[(i * win->channels + c) * win->frames + s], x = b.channels[c][s];
                            e_ref += r * r; e_out += x * x; e_diff += (x - r) * (x - r);
                        }
                    printf("  block %zu: continuous %.1f dB, resumed %.1f dB, difference %.1f dB (max %.1f dBFS)\n", i,
                           10 * log10(e_ref + 1e-30), 10 * log10(e_out + 1e-30), 10 * log10(e_diff + 1e-30), 20 * log10f(diff));
                }
                if (diff > 0) {
                    sum->differing++;
                    sum->last_differing = b.input_access_unit;
                    sum->worst = fmaxf(sum->worst, diff);
                }
            }
            if (render) render_block(render, &b);
        }
    }
    return DTSX_OK;
}

static int compare(const char *a_path, const char *b_path, size_t from_frame) {
    Input a = load(a_path), b = load(b_path);
    size_t skip = from_frame * OUT_CHANNELS, n = (a.size < b.size ? a.size : b.size) / sizeof(float);
    const float *x = (const float *)a.data, *y = (const float *)b.data;
    size_t differing = 0;
    float worst = 0;
    for (size_t i = skip; i < n; i++)
        if (x[i] != y[i]) {
            differing++;
            worst = fmaxf(worst, fabsf(x[i] - y[i]));
        }
    if (a.size != b.size) {
        printf("  renders differ in length: %zu and %zu bytes\n", a.size, b.size);
        return 1;
    }
    if (differing == 0) {
        printf("  bit-identical from input frame %zu (%zu samples)\n", from_frame, n - skip);
        return 0;
    }
    printf("  identical from input frame %zu but for %zu of %zu samples, by at most %.3g\n", from_frame,
           differing, n - skip, worst);
    return 1;
}

int main(int argc, char **argv) {
    if (argc == 5 && !strcmp(argv[1], "--compare")) return compare(argv[2], argv[3], strtoul(argv[4], NULL, 10));
    size_t chunk = 4096;
    bool random = false, seek = false;
    const char *render_path = NULL;
    int expect_refusal = 0, expect_presentation = -1;
    const char *path = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--chunk") && i + 1 < argc) chunk = strtoul(argv[++i], NULL, 10);
        else if (!strcmp(argv[i], "--random-chunks")) random = true;
        else if (!strcmp(argv[i], "--render") && i + 1 < argc) render_path = argv[++i];
        else if (!strcmp(argv[i], "--expect-refusal") && i + 1 < argc) expect_refusal = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--expect-presentation") && i + 1 < argc) expect_presentation = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--seek")) seek = true;
        else path = argv[i];
    }
    if (!path || chunk == 0) {
        fprintf(stderr, "usage: harness [--chunk N | --random-chunks] [--render out.f32] "
                        "[--expect-refusal S] [--expect-presentation P] [--seek] <stream.dts>\n");
        return 2;
    }
    Input in = load(path);
    printf("%s, %zu bytes; %s\n", path, in.size, dtsx_version_string());
    CHECK(dtsx_selftest() == DTSX_OK, "selftest");

    DTSXDecoder *d = dtsx_decoder_create();
    Summary sum = {0};
    size_t *offsets = NULL, frames = 0;
    if (seek) {
        frames = frame_offsets(&in, &offsets);
        sum.window_count = getenv("HARNESS_SEEKS") ? atoi(getenv("HARNESS_SEEKS")) : 3;
        sum.windows = calloc((size_t)sum.window_count, sizeof(Window));
        for (int w = 0; w < sum.window_count; w++)
            sum.windows[w].start = frames * (size_t)(w + 1) / (size_t)(sum.window_count + 1);
    }
    Renderer renderer = {0};
    if (render_path) {
        renderer.out = fopen(render_path, "wb");
        renderer.buffer = malloc(sizeof(float) * OUT_CHANNELS * DTSX_MAX_BLOCK_FRAMES);
        if (!renderer.out) {
            perror(render_path);
            return 2;
        }
    }
    DTSXStatus st = decode(d, &in, 0, chunk, random, &sum, render_path ? &renderer : NULL, expect_presentation);
    const char *error = dtsx_decoder_last_error(d);

    if (expect_refusal) {
        CHECK(st == expect_refusal, "expected refusal %d, got %d (%s)", expect_refusal, st, error);
        CHECK(sum.blocks == 0, "%" PRIu64 " blocks before the refusal", sum.blocks);
        uint8_t byte = 0;
        DTSXBlock b;
        CHECK(dtsx_decoder_push(d, &byte, 1, DTSX_NO_PTS) == expect_refusal, "push after refusal");
        CHECK(dtsx_decoder_pull(d, &b) == expect_refusal, "pull after refusal");
        CHECK(dtsx_decoder_reset(d) == DTSX_OK, "reset after refusal");
        CHECK(dtsx_decoder_pull(d, &b) == expect_refusal, "refusal survives reset");
        printf("refused with %d (%s)\n", st, error);
        dtsx_decoder_destroy(d);
        return failures ? 1 : 0;
    }
    CHECK(st == DTSX_OK, "decode stopped with %d (%s)", st, error);

    DTSXStreamInfo info;
    CHECK(dtsx_decoder_get_stream_info(d, &info) == DTSX_OK, "stream info");
    DTSXStats stats;
    dtsx_decoder_get_stats(d, &stats);
    printf("presentation %s%s, %u channels at %u Hz, alternate marker %u\n",
           dtsx_presentation_name(info.presentation), info.experimental ? " (experimental)" : "",
           info.channel_count, info.sample_rate, info.alternate_marker);
    printf("first block at input frame %" PRIu64 "\n", sum.first_input);
    printf("blocks %" PRIu64 ", frames %" PRIu64 " (%.3f s), metadata updates %" PRIu64
           ", bed as authored %" PRIu64 ", decode errors %" PRIu64 ", skipped %" PRIu64 "\n",
           sum.blocks, sum.frames, sum.rate ? (double)sum.frames / sum.rate : 0.0, sum.updates,
           sum.bed_as_authored, stats.decode_errors, stats.access_units_skipped);
    CHECK(sum.blocks > 0, "no blocks");
    if (seek) CHECK(frames == sum.blocks + stats.access_units_skipped, "%zu frames parsed, %" PRIu64 " decoded", frames, sum.blocks);
    CHECK(sum.layouts == 1, "%" PRIu64 " layout changes", sum.layouts);
    CHECK(sum.discontinuities == 1, "%" PRIu64 " discontinuities", sum.discontinuities);
    CHECK(sum.bed_as_authored == 0, "%" PRIu64 " blocks played their bed as authored", sum.bed_as_authored);
    CHECK(stats.decode_errors == 0 && stats.sync_errors == 0, "decode errors %" PRIu64 ", sync errors %" PRIu64,
          stats.decode_errors, stats.sync_errors);
    if (render_path) {
        CHECK(!renderer.unsupported, "a channel the 7.1.4 render has no speaker for");
        fclose(renderer.out);
    }

    if (seek) {
        for (int w = 0; w < sum.window_count; w++) {
            Window *win = &sum.windows[w];
            CHECK(dtsx_decoder_reset(d) == DTSX_OK, "reset");
            /* Push from the frame's first byte up to the end of the window. */
            size_t end = win->start + WINDOW_BLOCKS < frames ? offsets[win->start + WINDOW_BLOCKS] : in.size;
            Input part = {in.data, end};
            Summary resumed = {.resumed_from = win};
            st = decode(d, &part, offsets[win->start], chunk, random, &resumed, NULL, expect_presentation);
            CHECK(st == DTSX_OK, "resume at frame %" PRIu64 " stopped with %d", win->start, st);
            printf("resume at frame %" PRIu64 ": %" PRIu64 " blocks, %" PRIu64 " differ from the continuous decode, "
                   "the last at block %" PRIu64 ", by at most %.1f dBFS\n", win->start, resumed.blocks,
                   resumed.differing, resumed.differing ? resumed.last_differing : 0,
                   resumed.differing ? 20 * log10f(resumed.worst) : -INFINITY);
            DTSXStats resumed_stats;
            dtsx_decoder_get_stats(d, &resumed_stats);
            CHECK(resumed.blocks + resumed_stats.access_units_skipped == WINDOW_BLOCKS || end == in.size,
                  "resumed blocks %" PRIu64 " + skipped %" PRIu64, resumed.blocks, resumed_stats.access_units_skipped);
            /* The core's filter and predictor history restarts empty, so its first frames
               reconstruct a little differently; it must converge, and stay quiet meanwhile. */
            CHECK(resumed.differing == 0 || resumed.last_differing < 32, "resume did not converge");
            CHECK(resumed.worst <= 0.03f, "resume differs by %.1f dBFS", 20 * log10f(resumed.worst));
        }
        free(offsets);
    }
    dtsx_decoder_destroy(d);
    for (int w = 0; w < sum.window_count; w++) free(sum.windows[w].planes);
    free(sum.windows);
    free(in.data);
    if (failures) printf("%d check(s) failed\n", failures);
    return failures ? 1 : 0;
}
