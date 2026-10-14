// Kitae Native Renderer — stable C ABI between the Rust engine and the C++/SDL2 shell.
//
// The C++ side owns: the OS window, the browser chrome (address bar, back/forward/
// reload), text rasterization via SDL_ttf, the event queue, and painting a flat
// display list pushed in by the Rust engine. The Rust side owns everything else:
// networking, HTML, CSS, style cascade, and layout.
//
// Coordinates: logical points (DPI-scaled internally). Page items are in page
// space; the renderer applies the current scroll offset and clips to the page
// viewport below the chrome.
#ifndef KITAE_RENDERER_H
#define KITAE_RENDERER_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct KitaeRenderer KitaeRenderer;

typedef struct {
    float x, y, w, h;
    uint32_t color; // 0xRRGGBBAA (exact for solid paint, first stop for
                    // gradients, fallback for image paint)
    float radius[4];  // px, TL TR BR BL (uniform zero = square)
    uint32_t paint_tag; // 0 solid, 1 linear gradient, 2 radial gradient,
                        // 3 background-image (paints as `color` in L scope),
                        // 4 conic gradient (B5)
    float angle;        // linear/conic: degrees clockwise from "to top"
    uint32_t nstops;    // gradient stops, ≤ 8
    float offsets[8];   // 0..1 along the gradient line (conic: the sweep)
    uint32_t colors[8]; // 0xRRGGBBAA per stop
    float rcx, rcy;     // radial/conic center, fractions of the box
    float rradius;      // radial radius px, < 0 = farthest-corner
    float m[6];         // item-local 2×3 row-major matrix (Q2):
                        // x' = m[0]·x + m[1]·y + m[2],
                        // y' = m[3]·x + m[4]·y + m[5]
    int transformed;    // 0 = identity (fast path), 1 = paint through m[]
} KitaeDLRect;

typedef struct {
    float x, y;       // baseline origin
    const char* text; // UTF-8; copied on submit
    float size;       // logical point size
    uint32_t weight;  // 400 = regular, 700 = bold
    int italic;
    int monospace;    // fixed-width face (pre/code)
    uint32_t color;   // 0xRRGGBBAA
    float letter_spacing; // extra px after each glyph except the last
                          // (Q3); 0 = draw the run as one texture
    float m[6];       // item-local 2×3 row-major matrix (Q2), see KitaeDLRect
    int transformed;
} KitaeDLText;

// A decoded image raster (J3/J5): the engine resolves the frame's payload id
// against its raster side-table and hands the RGBA bytes here. Pixels are
// copied on submit; the shell caches textures keyed on (payload, pixel
// content) so a stable payload id with swapped pixels — a video's stepped
// frame — still re-uploads.
typedef struct {
    float x, y, w, h; // page-space box
    uint32_t fit;     // 0 fill, 1 contain, 2 cover, 3 none, 4 scale-down
    uint64_t payload; // raster id (cache key seed)
    uint32_t width;   // pixel size of `pixels`
    uint32_t height;
    const uint8_t* pixels; // width*height*4 RGBA8, row-major, straight alpha
    float m[6];      // item-local 2×3 row-major matrix (Q2), see KitaeDLRect
    int transformed;
} KitaeDLImage;

typedef enum {
    KITAE_EVENT_NONE = 0,
    KITAE_EVENT_QUIT,
    KITAE_EVENT_NAVIGATE, // url[0] filled
    KITAE_EVENT_BACK,
    KITAE_EVENT_FORWARD,
    KITAE_EVENT_RELOAD,
    KITAE_EVENT_SCROLL,   // a = delta y (points)
    KITAE_EVENT_RESIZE,   // a = viewport width, b = viewport height (page area)
    KITAE_EVENT_CLICK,    // a = x, b = y (page coords, scroll included)
} KitaeEventKind;

typedef struct {
    int32_t kind;
    float a, b;
    char url[4096];
} KitaeEvent;

// Lifecycle -------------------------------------------------------------
KitaeRenderer* kitae_renderer_init(int width, int height, const char* title);
void kitae_renderer_set_title(KitaeRenderer* r, const char* title);
// Reflects the engine's current URL into the chrome address bar.
void kitae_renderer_set_address(KitaeRenderer* r, const char* url);
// Size of the page viewport (window minus chrome), logical points.
void kitae_renderer_viewport(const KitaeRenderer* r, int* w, int* h);

// Display list ----------------------------------------------------------
// Submit the full list after any change. The renderer keeps its own copy.
void kitae_renderer_clear_list(KitaeRenderer* r);
void kitae_renderer_push_rect(KitaeRenderer* r, const KitaeDLRect* rect);
void kitae_renderer_push_text(KitaeRenderer* r, const KitaeDLText* text);
// Box shadows paint under their boxes (outset; inset is accepted but paints
// as an outset approximation in the SDL shell — GPU/Swift do true inset).
typedef struct {
    float x, y, w, h; // border box
    float radius[4];
    uint32_t color; // 0xRRGGBBAA
    float blur, spread, offset_x, offset_y;
    int inset;
    float m[6];      // item-local 2×3 row-major matrix (Q2), see KitaeDLRect
    int transformed;
} KitaeDLShadow;
void kitae_renderer_push_shadow(KitaeRenderer* r, const KitaeDLShadow* shadow);
void kitae_renderer_push_image(KitaeRenderer* r, const KitaeDLImage* img);
// Scroll state for painting + scrollbar (page coords).
void kitae_renderer_set_scroll(KitaeRenderer* r, float scroll_y, float content_height);

// Text metrics (for the Rust layout engine) ------------------------------
float kitae_renderer_measure_text(const KitaeRenderer* r, const char* text,
                                  float size, uint32_t weight, int italic,
                                  int monospace);
void kitae_renderer_font_metrics(const KitaeRenderer* r, float size,
                                 int monospace,
                                 float* ascent, float* descent, float* line_gap);

// Event pump + frame presentation ---------------------------------------
// Pumps SDL events one at a time (returns 1 with the event filled in).
// When the queue is empty, paints one frame and returns 0.
int kitae_renderer_poll_event(KitaeRenderer* r, KitaeEvent* out);

void kitae_renderer_destroy(KitaeRenderer* r);

// Implemented by the Rust app (same final binary): is there a link at
// viewport point (x, y)? Used to pick the mouse cursor.
bool kitae_app_hit_link(float x, float y);

#ifdef __cplusplus
}
#endif

#endif // KITAE_RENDERER_H
