/*
 * kitae_core.h — the C contract between the Kitae engine (libkitae_core.a,
 * built from the crates/kitae_core Rust crate) and non-Rust hosts such as
 * the Swift macOS app.
 *
 * Model
 * -----
 *  * The host owns everything OS-side: run loop, windows, chrome, scrolling,
 *    painting, opening external URLs.
 *  * The engine runs on its own thread inside kitae_engine_new. Commands go
 *    in (kitae_engine_command), finished frames come out (kitae_engine_frame).
 *    Text measurement is injected: the engine calls the host's vtable
 *    synchronously whenever layout needs a glyph advance — one source of
 *    truth for widths on both sides.
 *  * All functions are thread-safe except that each engine must be created,
 *    used and destroyed by the thread that owns it (the host main thread).
 *
 * Frame wire format (little-endian, no padding — read sequentially)
 * -----------------------------------------------------------------
 *   u32   version (20)
 *   u32   flags            bit0 can_go_back, bit1 can_go_forward,
 *                          bit2 print_media (v20; the page is laid out for
 *                          print media — see KITAE_CMD_SET_MEDIA)
 *   f32   scroll_y         engine-tracked scroll (informational)
 *   f32   content_height   total document height in points
 *   u32   page_background  0xRRGGBBAA
 *   u32   url_len; url_len bytes UTF-8
 *   u32   title_len; title_len bytes UTF-8
 *   u32   rect_count
 *   u32   text_count
 *   u32   link_count
 *   u32   image_count      (v2+; v1 frames have no count or images)
 *   rect_count × { f32 x, y, w, h; u32 color;   (0xRRGGBBAA)
 *                  f32 radius_tl, radius_tr, radius_br, radius_bl;  (v7+)
 *                  u8 paint_tag;                                    (v7+)
 *                  tag 0: solid (color above is exact);
 *                  tag 1: linear gradient — f32 angle_deg (clockwise
 *                         from "to top"), u32 nstops, nstops ×
 *                         { f32 offset 0..1; u32 color };
 *                  tag 2: radial gradient — f32 cx, cy (fractions),
 *                         u8 has_radius + [f32 radius_px], u32 nstops,
 *                         nstops × { f32 offset; u32 color };
 *                  tag 3: background-image — u32 url_len; url bytes
 *                         (paints as `color` in L scope);
 *                  tag 4: conic gradient — f32 angle_deg (clockwise from
 *                         "to top"), f32 cx, cy (fractions), u32 nstops,
 *                         nstops × { f32 offset; u32 color } }
 *   text_count  × { f32 x, y (baseline), size; u32 weight, italic,
 *                   monospace, color; f32 letter_spacing (v10+; 0 = draw
 *                   the run normally, else per-char advance + spacing);
 *                   u32 family_len; family bytes UTF-8 (v14+; first
 *                   non-generic font-family, "" = default);
 *                   u32 len; len bytes UTF-8 }
 *   link_count  × { f32 x, y, w, h; u32 len; len bytes UTF-8 }  (absolute href)
 *   image_count × { f32 x, y (top-left), w, h; u32 fit (0 fill, 1 contain,
 *                   2 cover, 3 none, 4 scale-down);
 *                   u32 src_len; src bytes (absolute URL);
 *                   u32 alt_len; alt bytes;
 *                   u64 payload;   (v4+; 0 = placeholder, else resolve the
 *                   decoded RGBA raster through kitae_qtsq_raster)
 *                   u8 animated    (v13+; 1 = multi-frame animation) }
 *   u32   focused_len; focused bytes UTF-8   (v3+; "" = no focus)
 *   u32   selection_len; selection bytes UTF-8 (v3+; "" = no selection)
 *   u8    media_present     (v5+; 0 = no media, nothing follows)
 *   media_present == 1:
 *     u8   kind            (0 audio, 1 video)
 *     f32  duration_secs
 *     u32  sample_rate     (audio; 0 for video)
 *     u8   channels        (audio; 0 for video)
 *     u8   playing         (transport mirror)
 *     f32  position_secs   (transport mirror)
 *     u32  url_len; url bytes UTF-8        (absolute media document URL)
 *     u32  caf_len; caf bytes UTF-8        (temp CAF path for audio, ""
 *                                          for video — the engine decodes;
 *                                          the embedder plays)
 *   u32   media_count      (v6+; media items in page order)
 *   media_count × { u64 id; f32 x, y, w, h; u32 src_len; src bytes
 *                   (absolute media document URL); u8 kind (0 audio,
 *                   1 video); u8 playing; f32 position_secs;
 *                   u8 controls (v20+; 1 = element carries `controls`) }
 *                   — a `<video>` element's rect + transport mirror. Its
 *                   current frame paints through the sibling image entry
 *                   whose payload references the stepped raster.
 *   u32   shadow_count     (v7+; box shadows, parents before children)
 *   shadow_count × { f32 x, y, w, h (border box); f32 radius ×4;
 *                    u32 color; f32 blur, spread, offset_x, offset_y;
 *                    u8 inset }
 *   u32   find_count       (v8+; find-in-page matches in page order)
 *   u32   find_active      (v8+; active match index, 0xFFFFFFFF = none)
 *   find_count × { u32 rect_count; rect_count × { f32 x, y, w, h } }
 *                   (page-space highlight boxes, one per text run touched;
 *                   the embedder overlays them — dim for matches, strong
 *                   for find_active)
 *   u32   ax_count         (v8+; accessibility tree, pre-order flattening)
 *   ax_count × { u32 id_len; id bytes UTF-8 (element id, else "ax{N}");
 *                u8 role (0 document, 1 heading, 2 link, 3 button,
 *                4 checkbox, 5 textbox, 6 combobox, 7 image, 8 table,
 *                9 text, 10 group, 11 live region, 12 radio button,
 *                13 text area, 14 slider, 15 spin button, 16 progress,
 *                17 color well, 18 list, 19 list item, 20 navigation,
 *                21 main, 22 banner, 23 content info, 24 complementary,
 *                25 search, 26 status, 27 alert, 28 article, 29 form,
 *                30 separator, 31 switch, 32 radio group, 33 toolbar,
 *                34 row, 35 cell, 36 column header, 37 row header,
 *                38 dialog, 39 disclosure triangle, 40 term,
 *                41 definition, 42 timer, 43 scroll bar);
 *                u32 label_len; label bytes UTF-8;
 *                f32 x, y, w, h (page space; 0×0 = unresolved);
 *                u32 parent (index into this list, 0xFFFFFFFF = root) }
 *   u32   ax_focused_len; ax_focused bytes UTF-8  (v8+; "" = none)
 *   u8    qtsq_locked      (v9+; 1 = page served from a locked archive)
 *   u8    qtsq_sig         (v11+; 0 unsigned, 1 valid, 2 invalid,
 *                          3 unverified)
 *   u32   transform_count  (v12+; sparse item-local 2D transforms, Q2)
 *   transform_count × { u8 kind (0 rect, 1 text, 2 image, 3 shadow,
 *                       4 link);
 *                       u32 index (into that item list);
 *                       f32 m0..m5 (row-major 2×3: x' = m0·x + m1·y + m2,
 *                       y' = m3·x + m4·y + m5) }
 *                       (kind 4 is additive within v20; older readers skip
 *                       the complete 29-byte entry)
 *   u32   sticky_count     (v15+; position:sticky side-list, Q5)
 *   sticky_count × { per item list (rect, text, link, image, media,
 *                    shadow) { u32 start; u32 count };  (6 pairs of
 *                    indices into those lists)
 *                    f32 base_y, height, container_top, container_bottom,
 *                    top_offset, bottom_offset; u8 has_top; u8 has_bottom }
 *                    (layout paints these boxes at their flow position;
 *                    the host shifts them at paint time — and the engine's
 *                    hit-test matches)
 *   u32   filter_count     (v16+; sparse per-item `filter` chains)
 *   filter_count × { u8 kind (0 rect, 1 text, 2 image, 3 shadow);
 *                    u32 index; u32 fn_count; fn_count × function }
 *                    function: u8 tag + args — 0 blur { f32 radius };
 *                    1 drop-shadow { f32 dx, dy, blur; u32 color };
 *                    2 brightness { f32 }; 3 contrast { f32 };
 *                    4 grayscale { f32 }; 5 hue-rotate { f32 deg };
 *                    6 invert { f32 }; 7 opacity { f32 };
 *                    8 saturate { f32 }; 9 sepia { f32 }
 *   u32   scroll_count     (v17+; scrollable containers, B2)
 *   scroll_count × { u32 node; f32 x, y, w, h (viewport rect, page
 *                    space); f32 content_height }
 *   optional B3 metadata after the v19 blur tail when scroll_count > 0:
 *     u32 entry_count
 *     entry_count × { f32 content_width; u8 mode (1 auto, 2 scroll);
 *                     u32 parent_node (0xFFFFFFFF = none) }
 *   u32   clip_count       (v18+; sparse per-item clip-path/mask (B7)
 *                           + blend/isolation (B8))
 *   clip_count × { u8 kind (0 rect, 1 text, 2 image, 3 shadow);
 *                  u32 index; clip descriptor; u8 blend; u32 group }
 *                  descriptor: u8 tag —
 *                    0 none       (blend/group-only entry — B8)
 *                    1 circle   { f32 cx, cy, radius }
 *                    2 ellipse  { f32 cx, cy, rx, ry }
 *                    3 inset    { f32 x, y, w, h; corners tl, tr, br,
 *                                 bl { f32 hr, vr } }
 *                    4 polygon  { u8 evenodd; u32 n; n × { f32 x, y } }
 *                  (geometry in page space, like everything else)
 *                  blend: mix-blend-mode wire value —
 *                    0 normal, 1 multiply, 2 screen, 3 overlay,
 *                    4 darken, 5 lighten, 6 color-dodge, 7 color-burn,
 *                    8 hard-light, 9 soft-light, 10 difference,
 *                    11 exclusion, 12 hue, 13 saturation, 14 color,
 *                    15 luminosity
 *                  group: nonzero = the item paints inside an
 *                    `isolation: isolate` subtree (stable-per-frame id);
 *                    the host wraps a contiguous run of equal ids in an
 *                    offscreen layer so blends stay inside.
 *   u32   blur_count      (v19+; sparse per-text shadow blur, E3)
 *   blur_count × { u32 index (into the text list); f32 blur }
 *                  (only runs with blur > 0 appear; a blurred run is an
 *                  R8 text-shadow duplicate — its color already carries
 *                  the shadow color, the host blurs the alpha it draws,
 *                  e.g. Core Text via NSShadow)
 *   optional AX semantics tail, after any v20 scroll metadata:
 *     u32 magic = 0x31535841 (wire bytes "AXS1")
 *     u32 version = 1
 *     u32 payload_length (bytes after this field)
 *     u32 entry_count
 *     entry_count × { u32 node_index; u8 flags; u8 checked;
 *                     u8 heading_level; u8 live_politeness;
 *                     f32 value_min; f32 value_max;
 *                     u32 value_len; value_len bytes UTF-8 }
 *     flags: bit0 disabled, bits1/2 expanded present/value,
 *            bits3/4 selected present/value, bit5 value present,
 *            bit6 value_min present, bit7 value_max present
 *     checked: 0 unspecified, 1 unchecked, 2 checked, 3 mixed
 *     live_politeness: 0 off, 1 polite, 2 assertive
 *   The tail is sparse. When it follows an empty scroll list, the engine
 *   writes a legacy u32 zero scroll count before the magic so older v20
 *   decoders can stop safely; absent tail means all semantics are default.
 *
 * Wire format v20 (F2/F4): media items gain a trailing u8 `controls` flag
 * (1 = the element carries the `controls` attribute). The engine then
 * demuxes that video into the web-media store and paints + hit-tests the
 * builtin transport chrome itself; the host only drives the decode loop
 * through the web-video C API below. QTSQ viewer videos keep 0 — their
 * transport stays in the v5 media section / the host's docked bar.
 *
 * Print media (v20): KITAE_CMD_SET_MEDIA switches the page's media
 * environment. In print media `@media print` rules apply, `@media screen`
 * ones do not, and the `prefers-*` features answer as a print device
 * (light scheme, reduced motion, normal contrast) whatever the screen is
 * set to. The engine re-lays-out and republishes with `flags` bit2 set, so
 * a host can hand that frame to the print system and send command 52 with
 * `a = 0` to get the screen frame back. Every frame section, size and field
 * is unchanged from the other v20 frames — bit2 was a reserved bit inside
 * the existing flags word, so v20 readers that ignore it decode a print
 * frame exactly as they would a screen one.
 *
 * Rectangles, text runs and images are in page space (origin = top-left of
 * the document, y grows downward). The host adds its local scroll offset
 * when painting and translates clicks back the same way. Transformed items,
 * including links, paint or hit-test their *stored* page-space geometry
 * through the matrix; media is untransformed.
 */

#ifndef KITAE_CORE_H
#define KITAE_CORE_H

#include <stddef.h>
#include <stdint.h>

enum {
    KITAE_FRAME_VERSION = 20,
    KITAE_AX_SEMANTICS_VERSION = 1,
    KITAE_AX_SEMANTICS_MAGIC = 0x31535841
};

#ifdef __cplusplus
extern "C" {
#endif

/* Command kinds for kitae_engine_command. */
enum {
    KITAE_CMD_NAVIGATE = 1,
    KITAE_CMD_BACK     = 2,
    KITAE_CMD_FORWARD  = 3,
    KITAE_CMD_RELOAD   = 4,
    KITAE_CMD_SCROLL   = 5, /* a = delta y (points)                */
    KITAE_CMD_RESIZE   = 6, /* a = width, b = height (points)      */
    KITAE_CMD_CLICK    = 7, /* a = x, b = y, page space            */
    KITAE_CMD_FOCUS_NEXT = 8,
    KITAE_CMD_FOCUS_PREV = 9,
    KITAE_CMD_ACTIVATE = 10,
    KITAE_CMD_SET_INPUT = 11, /* url = "id\nvalue"                */
    KITAE_CMD_SUBMIT = 12,    /* url = form id                     */
    KITAE_CMD_SELECT_ALL = 13,
    KITAE_CMD_CLEAR_SELECTION = 14,
    KITAE_CMD_MEDIA_OPEN = 15,   /* reply = fresh frame carrying media state  */
    KITAE_CMD_MEDIA_PLAY = 16,   /* a = playing (0/1), b = position seconds   */
    KITAE_CMD_MEDIA_STATE = 17,  /* query; reply = fresh frame (same as 15)   */
    KITAE_CMD_HOVER = 18,        /* url = element id (empty clears)           */
    KITAE_CMD_ACTIVE = 19,       /* url = element id (empty clears)           */
    KITAE_CMD_FIND = 20,         /* url = find query (empty clears)           */
    KITAE_CMD_FIND_NEXT = 21,    /* step the active match forward (wrapping)  */
    KITAE_CMD_FIND_PREV = 22,    /* step the active match backward (wrapping) */
    KITAE_CMD_FIND_CLEAR = 23,   /* clear the query, matches and highlight    */
    KITAE_CMD_SET_PASSWORD = 24, /* url = QTSQ password (session + reload)    */
    KITAE_CMD_SET_ENVIRONMENT = 25,
        /* a = color scheme (0 light, 1 dark);
         * b = bits: 1 reduce-motion, 2 more-contrast (@media prefers-*) */
    KITAE_CMD_SAVE_PAGE = 26,    /* url = destination path; outcome arrives    */
                                 /* later on the notifications queue           */
    KITAE_CMD_LIVE_POLL = 27,    /* drain a mounted kitae_live socket once     */
    KITAE_CMD_LIVE_STATE = 28,   /* reply via notifications: "live\t1" if the  */
                                 /* page is a connected Live session, else 0   */
    KITAE_CMD_ESCAPE = 29,       /* close the topmost open modal dialog        */
    KITAE_CMD_STEP_FOCUS = 30,   /* a = direction (+1/-1); range/number/select */
    KITAE_CMD_TYPE = 31,         /* url = text to insert at the caret (A11a)   */
    KITAE_CMD_BACKSPACE = 32,    /* delete the char before the caret (A11a)    */
    KITAE_CMD_DELETE_FORWARD = 33, /* delete the char after the caret (A11a)   */
    KITAE_CMD_MOVE_CARET = 34,   /* a = char delta (negative = left) (A11a)    */
    KITAE_CMD_SET_CARET = 35,    /* a = absolute char offset, clamped (A11a)   */
    KITAE_CMD_SET_FILE_FILES = 36, /* H2: url = "id\npath1\tpath2…" (empty     */
                                 /* paths clear) — the open-panel answer for  */
                                 /* a clicked input[type=file]                */
    KITAE_CMD_SET_AUTH = 37,     /* H6: url = "user\npass" — the 401 sign-in  */
                                 /* answer; stores session credentials and    */
                                 /* re-fetches the challenged URL             */
    KITAE_CMD_INSPECT_PICK = 38, /* H5: a = x, b = y (page space); a hit      */
                                 /* arrives as "inspect\t{node}" on the       */
                                 /* notifications queue                       */
    KITAE_CMD_INSPECT_INFO = 39, /* H5: a = node id; the element + computed-  */
                                 /* style report arrives as "inspectinfo\t…"  */
                                 /* (multi-line text, no tabs)                */
    KITAE_CMD_SET_PRIVACY_POLICY = 40, /* a bits: 1 DNT, 2 block third-party   */
                                      /* cookies, 4 upgrade top-level HTTP,   */
                                      /* 8 warn before insecure navigation   */
    KITAE_CMD_SET_SEARCH_PROVIDER = 41, /* a: 0 DuckDuckGo, 1 Wiby,            */
                                       /* 2 Marginalia, 3 Wikipedia           */
    KITAE_CMD_RESOLVE_INSECURE_NAVIGATION = 42, /* a != 0 allows the request */
    KITAE_CMD_SELECTION_BEGIN = 43, /* a = offset in the focused plain-text value */
    KITAE_CMD_SELECTION_EXTEND = 44, /* a = offset in the focused plain-text value */
    KITAE_CMD_SELECTION_END = 45,   /* finish the active range               */
    KITAE_CMD_COMPOSITION_UPDATE = 46, /* url = current marked text            */
    KITAE_CMD_COMPOSITION_COMMIT = 47, /* url = committed text, or NULL         */
    KITAE_CMD_COMPOSITION_CANCEL = 48,
    KITAE_CMD_EXTEND_CARET = 49, /* a = signed character delta */
    KITAE_CMD_SELECTION_BEGIN_POINT = 50, /* x, y = page-space point */
    KITAE_CMD_SELECTION_EXTEND_POINT = 51, /* x, y = page-space point */
    KITAE_CMD_SET_MEDIA = 52,   /* a = media type (0 screen, 1 print). The page
                                 * is re-styled + re-laid-out for that media
                                 * (no reload, no refetch) and the reply is a
                                 * fresh frame whose `flags` bit2 says which
                                 * environment it carries. In print media the
                                 * `prefers-*` features answer as paper
                                 * (light, reduce-motion, normal contrast)
                                 * whatever the screen is set to. Host side:
                                 * print that frame, then send a = 0. */
    KITAE_CMD_SET_MARKED_TEXT = KITAE_CMD_COMPOSITION_UPDATE,
    KITAE_CMD_COMMIT_COMPOSITION = KITAE_CMD_COMPOSITION_COMMIT,
    KITAE_CMD_CANCEL_COMPOSITION = KITAE_CMD_COMPOSITION_CANCEL,
};

/* Host-provided text measurement, called synchronously from the engine
 * thread. Both functions must be safe to call from any thread (Core Text
 * measurement is). `measure` returns the advance width of `text` at `size`
 * points; `metrics` returns ascent/descent in points. `family` selects a
 * per-run web font (empty string = the engine's default family). */
typedef struct KitaeMeasurerVtable {
    void *ctx;
    float (*measure)(void *ctx, const char *text, float size, uint32_t weight,
                     int italic, int monospace, const char *family);
    void (*metrics)(void *ctx, float size, int monospace, float *ascent,
                    float *descent, const char *family);
    /* B9: x-height (the `ex` unit) of the face at `size`; NULL keeps the
     * 0.5em fallback. `family` is "" for the host default. */
    float (*x_height)(void *ctx, float size, uint32_t weight, int italic,
                      int monospace, const char *family);
} KitaeMeasurerVtable;

typedef struct KitaeEngine KitaeEngine;

/* Spawn an engine (and its thread) with the given page viewport in points
 * and the measurement vtable. The engine immediately loads "about:start".
 * Returns NULL if `vtable` is NULL. */
KitaeEngine *kitae_engine_new(int viewport_w, int viewport_h,
                              const KitaeMeasurerVtable *vtable);

/* Shut the engine down; the thread finishes any in-flight fetch first. */
void kitae_engine_destroy(KitaeEngine *engine);

/* Send a command (kinds above). `a`/`b` carry scroll deltas, click points or
 * viewport sizes; `url` carries the navigation target (ignored otherwise).
 * Selection commands use `a` as a character offset, or `a`/`b` as a
 * page-space point for the point-selection commands; `KITAE_CMD_EXTEND_CARET`
 * uses `a` as a signed character delta. Composition update uses `url` for
 * marked text; composition commit uses `url` for the final text, or NULL to
 * commit the current marked text. `KITAE_CMD_SET_MEDIA` uses `a` as the media
 * type (0 screen, 1 print) and republishes the page in it. */
void kitae_engine_command(KitaeEngine *engine, int kind, float a, float b,
                          const char *url);

/* Copy the newest published frame into `buf` (see the wire format above).
 * Returns the frame's full length; when `cap` is at least that, the frame is
 * copied and consumed, otherwise it stays pending (grow the buffer, call
 * again). Returns 0 when nothing new has been published. */
size_t kitae_engine_frame(KitaeEngine *engine, uint8_t *buf, size_t cap);
uint32_t kitae_frame_version(void);
int kitae_engine_frame_available(KitaeEngine *engine, size_t *out_len);

/* 1 while a navigation is in flight, 0 otherwise (loading hairline). */
int kitae_engine_loading(KitaeEngine *engine);

/* Pop one escape-hatch URL queued by the engine (kitae:open-external: /
 * mailto: targets). Returns 1 and NUL-terminates into `buf` (4 KiB
 * recommended), 0 when the queue is empty or `buf` is too small (the URL is
 * dropped). The host decides how to open it (NSWorkspace, `open`, …). */
int kitae_engine_take_external(KitaeEngine *engine, char *buf, size_t cap);

/* Pop one engine notification — currently Save-Page-as-QTSQ outcomes:
 * "1\t{path}" on success, "0\t{message}" on failure. Returns 1 and
 * NUL-terminates into `buf` (4 KiB recommended), 0 when the queue is empty
 * or `buf` is too small (the message is dropped). */
int kitae_engine_take_notification(KitaeEngine *engine, char *buf, size_t cap);

/* Registrable domain for address presentation. Returns the NUL-terminated
 * buffer length including NUL, or 0 when the input has no usable host. */
size_t kitae_registrable_domain(const char *url, char *out, size_t cap);

/* HSTS inspection and deletion. The expiry is the effective policy's remaining
 * lifetime in seconds, or 0 when no policy covers the URL. */
int kitae_hsts_covers(const char *url);
int64_t kitae_hsts_expires_in(const char *url);
int kitae_hsts_includes_subdomains(const char *url);
int kitae_clear_hsts(const char *url);
void kitae_clear_all_hsts(void);

/* HSTS snapshot format: u32 version (1), u32 count, then per entry u32 host
 * length, UTF-8 host,
 * u8 includeSubDomains, u64 absolute Unix expiry (little-endian). The size
 * query and copy return the required byte count; import replaces the store
 * with the validated snapshot and returns the number of accepted entries. */
size_t kitae_hsts_snapshot_size(void);
size_t kitae_hsts_snapshot(uint8_t *out, size_t cap);
int kitae_hsts_import_snapshot(const uint8_t *data, size_t len);

/* Decoded image rasters (J3). Frame image entries with payload != 0 reference
 * a raster in the engine's decoded-raster side-table; pull the pixels here.
 * kitae_qtsq_raster_size returns 8 + w*h*4 (0 when the id is unknown);
 * kitae_qtsq_raster copies u32 width, u32 height, then w*h*4 bytes RGBA8
 * (row-major, straight alpha) and returns 1, or 0 when the id is unknown or
 * `cap` is too small. Rasters stay valid until the raster cache is cleared
 * (history clear); copying is cheap (Arc refcount read). */
size_t kitae_qtsq_raster_size(uint64_t id);
int kitae_qtsq_raster(uint64_t id, uint8_t *buf, size_t cap);

/* Animated-raster frames (QTSQ GIF/WebP): `kitae_raster_frames` reports how
 * many frames the raster carries (0 = static); `kitae_raster_frame_delay`
 * gives frame `n`'s presentation delay in ms; `kitae_raster_loop` gives the
 * loop count (-1 = no animation, 0 = loop forever); `kitae_raster_frame`
 * copies frame `n` in the same 8-byte-header + RGBA8 shape as
 * kitae_qtsq_raster (returns 1, or 0 when unknown / cap too small). */
uint32_t kitae_raster_frames(uint64_t id);
uint32_t kitae_raster_frame_delay(uint64_t id, uint32_t n);
int kitae_raster_loop(uint64_t id);
int kitae_raster_frame(uint64_t id, uint32_t n, uint8_t *buf, size_t cap);

/* Purge the decoded image caches (I3): decoded rasters, animation frames
 * and the network byte cache. Everything re-decodes on demand, so this is
 * safe under memory pressure. Thread-safe; takes no engine handle. */
void kitae_purge_image_caches(void);

/* Web video decode surface (F2/F3): the engine demuxes `<video>` sources
 * (MP4/WebM/MKV) at layout time; the host's platform decoder (VideoToolbox)
 * pulls compressed samples, decodes, and feeds RGBA frames back. All
 * functions are thread-safe and keyed by the ABSOLUTE media URL (resolve
 * relative srcs against the page URL the way hrefs resolve). Codec ids:
 * 0 none/unknown, 1 H.264, 2 HEVC, 3 VP8, 4 VP9, 5 AV1. */
uint32_t kitae_web_video_codec(const char *url);
/* Track dimensions in px; writes *w and *h and returns 1, or 0 on miss. */
int kitae_web_video_dims(const char *url, uint32_t *w, uint32_t *h);
/* Track frame rate (fps; 0.0 on miss). */
double kitae_web_video_fps(const char *url);
/* Copy the codec extradata (avcC/hvcC/vpcC/av1C) into buf; returns the full
 * length (call with buf == NULL to size, then retry with a big enough
 * buffer). 0 on miss. */
unsigned long kitae_web_video_extradata(const char *url, uint8_t *buf,
                                        unsigned long cap);
/* Number of compressed samples in the track (0 on miss). */
uint32_t kitae_web_video_sample_count(const char *url);
/* Copy compressed sample `index` into buf; returns the full sample length
 * (0 on miss; buf == NULL sizes). Use kitae_web_video_sample_keyframe for
 * seeking. */
unsigned long kitae_web_video_sample(const char *url, uint32_t index,
                                     uint8_t *buf, unsigned long cap);
/* 1 when sample `index` is a sync/keyframe, else 0. */
int kitae_web_video_sample_keyframe(const char *url, uint32_t index);
/* Feed one decoded frame for `url` (BGRA order is fine — pass swizzled
 * RGBA8, row-major, straight alpha; len must be >= w*h*4). Registers the
 * pixels in the engine's raster cache under the media URL so the next
 * published frame stamps them into the `<video>` element's image entry.
 * Returns 1, or 0 on bad args / unknown state. */
int kitae_web_video_feed(const char *url, uint32_t w, uint32_t h,
                         const uint8_t *rgba, unsigned long len);

#ifdef __cplusplus
}
#endif

#endif /* KITAE_CORE_H */
