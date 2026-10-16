// Link-and-smoke test for the kitae_core C FFI: builds a C consumer against
// the staticlib, drives an engine over the start page, and validates the
// frame header. Implements the host measurer contract with a crude
// per-character estimate (the real host uses Core Text). Not part of any
// shipped binary.
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include "kitae_core.h"
/* B4: the rasterizer installer the app calls (it links the same static lib). */
#include "kitae_headless.h"

static int fail = 0;

static float c_measure(void *ctx, const char *text, float size, uint32_t weight,
                       int italic, int monospace, const char *family) {
    (void)ctx; (void)weight; (void)italic; (void)family;
    float per_char = monospace ? 0.6f * size : 0.5f * size;
    return (float)strlen(text) * per_char;
}

static void c_metrics(void *ctx, float size, int monospace, float *ascent,
                      float *descent, const char *family) {
    (void)ctx; (void)monospace; (void)family;
    *ascent = 0.8f * size;
    *descent = 0.2f * size;
}

static float c_x_height(void *ctx, float size, uint32_t weight, int italic,
                        int monospace, const char *family) {
    (void)ctx; (void)weight; (void)italic; (void)monospace; (void)family;
    return 0.5f * size;
}

int main(void) {
    char domain[64];
    size_t domain_len = kitae_registrable_domain(
        "https://www.example.co.uk/path", domain, sizeof domain);
    if (domain_len != strlen("example.co.uk") + 1 ||
        strcmp(domain, "example.co.uk") != 0) {
        puts("registrable domain mismatch");
        fail = 1;
    }

    KitaeMeasurerVtable vt;
    memset(&vt, 0, sizeof vt);
    vt.ctx = NULL;
    vt.measure = c_measure;
    vt.metrics = c_metrics;
    vt.x_height = c_x_height;

    KitaeEngine *e = kitae_engine_new(800, 600, &vt);
    if (!e) { puts("engine is NULL"); return 1; }

    unsigned char buf[1 << 20];
    size_t n = 0;
    for (int i = 0; i < 100; i++) {
        n = kitae_engine_frame(e, buf, sizeof buf);
        if (n > 0) break;
        usleep(20000);
    }
    printf("frame bytes: %zu\n", n);
    if (n < 32) { puts("frame too small"); fail = 1; }
    else {
        unsigned version = buf[0] | (buf[1] << 8) | (buf[2] << 16) | ((unsigned)buf[3] << 24);
        printf("version: %u\n", version);
        if (version != KITAE_FRAME_VERSION) fail = 1;
    }
    printf("loading: %d\n", kitae_engine_loading(e));

    /* B4: the agent plane, exactly as the app drives it — install the
     * rasterizer, run a read verb, then a write verb that must ask first. */
    if (!kitae_headless_install_raster(e, 1.0)) {
        puts("no system fonts — skipping the agent checks");
        kitae_engine_destroy(e);
        puts(fail ? "FAIL" : "OK");
        return fail;
    }
    kitae_engine_command(e, KITAE_CMD_AGENT_ACT, 0, 0, "{\"verb\":\"read\"}");
    /* 1 MiB: an inline screenshot's base64 is far bigger than a 4 KiB
     * notification, and take_notification leaves an oversized (or
     * unsized-for) message queued rather than dropping it. */
    static char note[1 << 20];
    int saw_read = 0, saw_pending = 0, saw_shot = 0;
    for (int i = 0; i < 200 && (!saw_read || !saw_pending || !saw_shot); i++) {
        size_t len = 0;
        if (kitae_engine_notification_available(e, &len) == 1 && len + 1 < sizeof note) {
            if (kitae_engine_take_notification(e, note, sizeof note) == 1) {
                if (!strncmp(note, "agent-result\t", 13)) {
                    if (strstr(note, "\"verb\":\"read\"") && strstr(note, "\"ok\":true"))
                        saw_read = 1;
                    if (strstr(note, "\"kind\":\"permission\""))
                        saw_pending = 1;
                }
                continue;
            }
        }
        /* read → navigate (must ask) → screenshot (read-only, needs the
         * rasterizer that was just installed). */
        if (saw_read && !saw_pending)
            kitae_engine_command(e, KITAE_CMD_AGENT_ACT, 0, 0,
                                 "{\"verb\":\"navigate\",\"url\":\"about:blank\"}");
        if (saw_read && saw_pending && !saw_shot)
            kitae_engine_command(e, KITAE_CMD_AGENT_ACT, 0, 0, "{\"verb\":\"screenshot\"}");
        usleep(20000);
    }
    /* …and the screenshot's own reply, which carries PNG bytes in base64. */
    for (int i = 0; i < 200 && !saw_shot; i++) {
        size_t len = 0;
        if (kitae_engine_notification_available(e, &len) == 1 && len + 1 < sizeof note
            && kitae_engine_take_notification(e, note, sizeof note) == 1
            && strstr(note, "\"mime\":\"image/png\"")) {
            saw_shot = 1;
            break;
        }
        usleep(20000);
    }
    printf("agent read=%d pending=%d screenshot=%d\n", saw_read, saw_pending, saw_shot);
    if (!saw_read || !saw_pending || !saw_shot) fail = 1;

    kitae_engine_destroy(e);
    puts(fail ? "FAIL" : "OK");
    return fail;
}
