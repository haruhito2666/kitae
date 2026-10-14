// Kitae Native Renderer: SDL2 window + chrome + rasterization behind the C ABI.
//
// Coordinate system: everything drawn is in logical points. The renderer uses
// SDL_RenderSetLogicalSize(window_w, window_h) so a Retina/HiDPI display only
// affects font rasterization size (size * dpi), never layout math.
#include "kitae_renderer.h"

#include <SDL2/SDL.h>
#include <SDL2/SDL_ttf.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <deque>
#include <string>
#include <unordered_map>
#include <vector>

namespace {

constexpr int kChromeHeight = 52;
constexpr uint32_t kChromeFg = 0xE8EAF0FF;
constexpr uint32_t kAddrFg = 0xBFC5D4FF;
constexpr uint32_t kScrollbar = 0x8A93A690;
constexpr uint32_t kFocusBorder = 0x5A8DFFFF;

SDL_Color ToSdlColor(uint32_t c) {
    SDL_Color out;
    out.r = (c >> 24) & 0xFF;
    out.g = (c >> 16) & 0xFF;
    out.b = (c >> 8) & 0xFF;
    out.a = c & 0xFF;
    return out;
}

// Open a font face. Helvetica.ttc and Menlo.ttc ship Regular(0) + Bold(1);
// fall back to face 0 with a synthesized style when a face is missing.
// Monospace text (pre/code) uses the platform's fixed-width face.
TTF_Font* OpenFont(int size_px, int weight, int italic, int monospace) {
    const char* path = monospace ? "/System/Library/Fonts/Menlo.ttc"
                                 : "/System/Library/Fonts/Helvetica.ttc";
    bool want_bold = weight >= 600;
    TTF_Font* f = nullptr;
    bool bold_face = false;
    if (want_bold) {
        f = TTF_OpenFontIndex(path, size_px, 1);
        bold_face = f != nullptr;
    }
    if (!f) f = TTF_OpenFontIndex(path, size_px, 0);
    if (!f && monospace) {
        // Menlo missing: fall back to Courier's fixed-width face.
        f = TTF_OpenFontIndex("/System/Library/Fonts/Courier.ttc", size_px, 0);
    }
    if (!f) return nullptr;
    int style = 0;
    if (want_bold && !bold_face) style |= TTF_STYLE_BOLD;
    if (italic) style |= TTF_STYLE_ITALIC;
    if (style) TTF_SetFontStyle(f, style);
    return f;
}

} // namespace

struct TextKey {
    std::string text;
    int size_px;
    int weight;
    int italic;
    int monospace;
    bool operator==(const TextKey& o) const {
        return size_px == o.size_px && weight == o.weight && italic == o.italic &&
               monospace == o.monospace && text == o.text;
    }
};

struct TextKeyHash {
    size_t operator()(const TextKey& k) const {
        size_t h = std::hash<std::string>()(k.text);
        h = h * 1315423911u + (size_t)k.size_px;
        h = h * 1315423911u + (size_t)k.weight;
        h = h * 1315423911u + (size_t)k.italic;
        h = h * 1315423911u + (size_t)k.monospace;
        return h;
    }
};

struct GlyphTexture {
    SDL_Texture* tex = nullptr;
    int w = 0, h = 0;
};

// Decoded-image cache entry: the RGBA raster uploaded as an SDL texture,
// keyed by payload id + pixel content hash so a re-stepped video frame
// (same payload id, new pixels) invalidates naturally.
struct ImageTexture {
    SDL_Texture* tex = nullptr;
    uint64_t hash = 0;
    uint32_t w = 0, h = 0;
};

struct KitaeRenderer {
    SDL_Window* window = nullptr;
    SDL_Renderer* ren = nullptr;

    int win_w = 0, win_h = 0;   // logical points
    float dpi = 1.0f;
    int page_w = 0, page_h = 0; // page viewport, logical points

    // Chrome state
    std::string addr_text;
    bool focus_addr = false;
    int hover_btn = -1; // 0 back, 1 fwd, 2 reload

    // Display list (page space, logical points). Text bytes live in
    // text_storage (stable pointers — deque never moves existing strings);
    // texts[i].text points into it. The Rust side passes temporary buffers.
    std::deque<std::string> text_storage;
    std::vector<KitaeDLRect> rects;
    std::vector<KitaeDLText> texts;
    std::vector<KitaeDLImage> images;

    std::vector<KitaeDLShadow> shadows;
    // RGBA bytes for images[i] live here (deque: stable backing pointers are
    // not needed — pixels are hashed + uploaded on the next paint — but the
    // deque keeps pushes O(1) and keeps earlier entries address-stable).
    std::deque<std::vector<uint8_t>> image_storage;
    float scroll_y = 0.0f;
    float content_h = 1.0f;

    std::unordered_map<std::string, TTF_Font*> font_cache;
    std::unordered_map<TextKey, GlyphTexture, TextKeyHash> text_cache;
    std::unordered_map<uint64_t, ImageTexture> image_cache;

    std::vector<KitaeEvent> pending;
};

// --- Paint upgrades: rounded rects, gradients, shadows (L4, CPU path) ---

static uint32_t LerpColor(uint32_t a, uint32_t b, float t) {
    if (t <= 0.0f) return a;
    if (t >= 1.0f) return b;
    uint32_t out = 0;
    for (int s = 24; s >= 0; s -= 8) {
        float ca = (float)((a >> s) & 0xFF), cb = (float)((b >> s) & 0xFF);
        out |= (uint32_t)(ca + (cb - ca) * t + 0.5f) << s;
    }
    return out;
}

// Gradient color at offset t (stops sorted by construction).
static uint32_t GradientAt(const float* offsets, const uint32_t* colors, uint32_t n,
                           float t, uint32_t fallback) {
    if (n == 0) return fallback;
    if (t <= offsets[0]) return colors[0];
    for (uint32_t i = 1; i < n; i++) {
        if (t <= offsets[i]) {
            float span = offsets[i] - offsets[i - 1];
            float k = span > 0.0f ? (t - offsets[i - 1]) / span : 1.0f;
            return LerpColor(colors[i - 1], colors[i], k);
        }
    }
    return colors[n - 1];
}

// Horizontal span inside a rounded box at page-space row y (clamped radii).
// Returns false when the row misses the box entirely.
static bool RoundedSpan(float x, float y, float w, float h, const float* radius, float row_y,
                        float* x0, float* x1) {
    if (row_y < y || row_y >= y + h) return false;
    float m = std::min(w, h) * 0.5f;
    float rtl = std::min(radius[0], m), rtr = std::min(radius[1], m);
    float rbr = std::min(radius[2], m), rbl = std::min(radius[3], m);
    float ly = row_y - y;
    float inset_l = 0.0f, inset_r = 0.0f;
    if (ly < rtl) {
        float d = rtl - ly;
        inset_l = std::max(inset_l, rtl - sqrtf(std::max(0.0f, rtl * rtl - d * d)));
    }
    if (ly < rtr) {
        float d = rtr - ly;
        inset_r = std::max(inset_r, rtr - sqrtf(std::max(0.0f, rtr * rtr - d * d)));
    }
    if (ly > h - rbr) {
        float d = ly - (h - rbr);
        inset_r = std::max(inset_r, rbr - sqrtf(std::max(0.0f, rbr * rbr - d * d)));
    }
    if (ly > h - rbl) {
        float d = ly - (h - rbl);
        inset_l = std::max(inset_l, rbl - sqrtf(std::max(0.0f, rbl * rbl - d * d)));
    }
    *x0 = x + inset_l;
    *x1 = x + w - inset_r;
    return *x1 > *x0;
}

static void FillRow(KitaeRenderer* r, float x0, float x1, float y, float sy, uint32_t color) {
    if (x1 <= x0) return;
    SDL_Color c = ToSdlColor(color);
    SDL_SetRenderDrawColor(r->ren, c.r, c.g, c.b, c.a);
    SDL_FRect dst{x0, kChromeHeight + (y - sy), x1 - x0, 1.0f};
    SDL_RenderFillRectF(r->ren, &dst);
}

// Linear gradient along its major axis (rows for vertical-ish, columns for
// horizontal-ish), clipped to the rounded box. Diagonal angles band along
// the major axis — exact for axis-aligned, close otherwise.
static void draw_linear(KitaeRenderer* r, const KitaeDLRect& rc, float sy, float view_bottom) {
    const float kPi = 3.14159265f;
    float rad = rc.angle * kPi / 180.0f;
    float dx = sinf(rad), dy = -cosf(rad); // y-down page space, 0deg = up
    // Project the four corners to find the gradient extent.
    float dots[4] = {rc.x * dx + rc.y * dy, (rc.x + rc.w) * dx + rc.y * dy,
                     rc.x * dx + (rc.y + rc.h) * dy, (rc.x + rc.w) * dx + (rc.y + rc.h) * dy};
    float mn = dots[0], mx = dots[0];
    for (int i = 1; i < 4; i++) {
        mn = std::min(mn, dots[i]);
        mx = std::max(mx, dots[i]);
    }
    float span = mx - mn;
    if (span <= 0.0f) {
        uint32_t c = GradientAt(rc.offsets, rc.colors, rc.nstops, 0.0f, rc.color);
        SDL_Color sc = ToSdlColor(c);
        SDL_SetRenderDrawColor(r->ren, sc.r, sc.g, sc.b, sc.a);
        SDL_FRect dst{rc.x, kChromeHeight + (rc.y - sy), rc.w, rc.h};
        SDL_RenderFillRectF(r->ren, &dst);
        return;
    }
    if (fabsf(dx) >= fabsf(dy)) {
        // Column-major.
        int x0 = (int)floorf(rc.x), x1 = (int)ceilf(rc.x + rc.w);
        for (int x = x0; x < x1; x++) {
            float t = ((x + 0.5f) * dx + (rc.y + rc.h * 0.5f) * dy - mn) / span;
            uint32_t c = GradientAt(rc.offsets, rc.colors, rc.nstops, t, rc.color);
            // Full-height column clipped by the rounded outline: walk rows.
            for (float y = floorf(std::max(rc.y, sy)); y < std::min(rc.y + rc.h, view_bottom);
                 y += 1.0f) {
                float sx0, sx1;
                if (RoundedSpan(rc.x, rc.y, rc.w, rc.h, rc.radius, y + 0.5f, &sx0, &sx1) &&
                    x + 1 > sx0 && x < sx1) {
                    FillRow(r, std::max((float)x, sx0), std::min((float)x + 1.0f, sx1), y, sy, c);
                }
            }
        }
        return;
    }
    for (float y = floorf(std::max(rc.y, sy)); y < std::min(rc.y + rc.h, view_bottom); y += 1.0f) {
        float t = ((rc.x + rc.w * 0.5f) * dx + (y + 0.5f) * dy - mn) / span;
        uint32_t c = GradientAt(rc.offsets, rc.colors, rc.nstops, t, rc.color);
        float sx0, sx1;
        if (RoundedSpan(rc.x, rc.y, rc.w, rc.h, rc.radius, y + 0.5f, &sx0, &sx1)) {
            FillRow(r, sx0, sx1, y, sy, c);
        }
    }
}

// B5 conic gradient: per-pixel sweep angle around the center. Stop offsets
// map linearly onto the 0–1 turn starting from `from angle` clockwise;
// offsets are NOT re-distributed (parse already anchored them).
static void draw_conic(KitaeRenderer* r, const KitaeDLRect& rc, float sy, float view_bottom) {
    const float kPi = 3.14159265f;
    float cx = rc.x + rc.rcx * rc.w;
    float cy = rc.y + rc.rcy * rc.h;
    float start = rc.angle; // clockwise degrees from "to top"
    for (float y = floorf(std::max(rc.y, sy)); y < std::min(rc.y + rc.h, view_bottom);
         y += 1.0f) {
        float sx0, sx1;
        if (!RoundedSpan(rc.x, rc.y, rc.w, rc.h, rc.radius, y + 0.5f, &sx0, &sx1)) continue;
        for (float x = floorf(sx0); x < sx1; x += 1.0f) {
            float px = x + 0.5f - cx, py = (y + 0.5f) - cy;
            float a = atan2f(px, -py) * 180.0f / kPi; // 0 = up, CW positive
            float rel = fmodf(a - start + 720.0f, 360.0f);
            float t = rel / 360.0f;
            FillRow(r, x, x + 1.0f, y, sy, GradientAt(rc.offsets, rc.colors, rc.nstops, t, rc.color));
        }
    }
}

// Radial gradient as back-to-front filled ellipses (one scanline span per
// row per band). Bands follow the stop list; the first band starts at the
// center.
static void draw_radial(KitaeRenderer* r, const KitaeDLRect& rc, float sy, float view_bottom) {
    float cx = rc.x + rc.rcx * rc.w;
    float cy = rc.y + rc.rcy * rc.h;
    float far = sqrtf(std::max({cx - rc.x, rc.x + rc.w - cx, 0.0f}) *
                          std::max({cx - rc.x, rc.x + rc.w - cx, 0.0f}) +
                      std::max({cy - rc.y, rc.y + rc.h - cy, 0.0f}) *
                          std::max({cy - rc.y, rc.y + rc.h - cy, 0.0f}));
    float rmax = rc.rradius >= 0.0f ? rc.rradius : far;
    if (rmax <= 0.0f) return;
    // Band edges in offset space: 0 + stop offsets + 1.
    float edges[10] = {0.0f};
    uint32_t n = 0;
    for (uint32_t i = 0; i < rc.nstops && n < 9; i++) edges[n++] = rc.offsets[i];
    edges[n++] = 1.0f;
    for (int b = (int)n - 1; b >= 1; b--) {
        float t1 = edges[b]; // outer edge of this band
        uint32_t c = GradientAt(rc.offsets, rc.colors, rc.nstops, t1, rc.color);
        float rx = rmax * t1, ry = rmax * t1;
        if (rx <= 0.0f || ry <= 0.0f) continue;
        for (float y = floorf(std::max(rc.y, sy)); y < std::min(rc.y + rc.h, view_bottom);
             y += 1.0f) {
            float ny = ((y + 0.5f) - cy) / ry;
            if (fabsf(ny) > 1.0f) continue;
            float hw = rx * sqrtf(std::max(0.0f, 1.0f - ny * ny));
            float sx0, sx1;
            if (!RoundedSpan(rc.x, rc.y, rc.w, rc.h, rc.radius, y + 0.5f, &sx0, &sx1)) continue;
            FillRow(r, std::max(cx - hw, sx0), std::min(cx + hw, sx1), y, sy, c);
        }
    }
}

static void draw_rect(KitaeRenderer* r, const KitaeDLRect& rc, float sy, float view_bottom) {
    if (rc.y + rc.h < sy || rc.y > view_bottom || rc.w <= 0.0f || rc.h <= 0.0f) return;
    if (rc.paint_tag == 1 && rc.nstops > 0) {
        draw_linear(r, rc, sy, view_bottom);
        return;
    }
    if (rc.paint_tag == 2 && rc.nstops > 0) {
        draw_radial(r, rc, sy, view_bottom);
        return;
    }
    if (rc.paint_tag == 4 && rc.nstops > 0) {
        draw_conic(r, rc, sy, view_bottom);
        return;
    }
    // Solid (and background-image fallback): rounded scanline fill.
    SDL_Color c = ToSdlColor(rc.color);
    SDL_SetRenderDrawColor(r->ren, c.r, c.g, c.b, c.a);
    bool square = true;
    for (int i = 0; i < 4; i++) {
        if (rc.radius[i] > 0.0f) {
            square = false;
            break;
        }
    }
    if (square) {
        SDL_FRect dst{rc.x, kChromeHeight + (rc.y - sy), rc.w, rc.h};
        SDL_RenderFillRectF(r->ren, &dst);
        return;
    }
    for (float y = floorf(std::max(rc.y, sy)); y < std::min(rc.y + rc.h, view_bottom); y += 1.0f) {
        float sx0, sx1;
        if (RoundedSpan(rc.x, rc.y, rc.w, rc.h, rc.radius, y + 0.5f, &sx0, &sx1)) {
            FillRow(r, sx0, sx1, y, sy, rc.color);
        }
    }
}

// Outset box shadows as layered alpha shells (inset arrives here only as an
// outset approximation — GPU/Swift do true inset). Blend is enabled for the
// pass and restored after so solid paints stay opaque.
static void draw_shadow(KitaeRenderer* r, const KitaeDLShadow& sh, float sy, float view_bottom) {
    float layers = std::min(16.0f, std::max(1.0f, ceilf(sh.blur)));
    SDL_SetRenderDrawBlendMode(r->ren, SDL_BLENDMODE_BLEND);
    for (int i = (int)layers; i >= 1; i--) {
        float k = (float)i / layers;
        float expand = sh.spread + sh.blur * k;
        uint32_t c = sh.color;
        float a = ((c & 0xFF) / 255.0f) * (1.0f - k) * (1.0f / layers) * 2.0f;
        if (a <= 0.004f) continue;
        uint32_t faded = (c & 0xFFFFFF00) | (uint32_t)(a * 255.0f + 0.5f);
        SDL_Color sc = ToSdlColor(faded);
        SDL_SetRenderDrawColor(r->ren, sc.r, sc.g, sc.b, sc.a);
        float x0 = sh.x + sh.offset_x - expand, y0 = sh.y + sh.offset_y - expand;
        float x1 = sh.x + sh.w + sh.offset_x + expand, y1 = sh.y + sh.h + sh.offset_y + expand;
        SDL_FRect dst{x0, kChromeHeight + (y0 - sy), x1 - x0, y1 - y0};
        if (dst.y + dst.h >= kChromeHeight && dst.y <= kChromeHeight + (view_bottom - sy)) {
            SDL_RenderFillRectF(r->ren, &dst);
        }
    }
    SDL_SetRenderDrawBlendMode(r->ren, SDL_BLENDMODE_NONE);
}

static void compute_page_viewport(KitaeRenderer* r) {
    int pw = 0, ph = 0;
    SDL_GetRendererOutputSize(r->ren, &pw, &ph);
    if (r->win_w > 0) r->dpi = (float)pw / (float)r->win_w;
    SDL_RenderSetLogicalSize(r->ren, r->win_w, r->win_h);
    r->page_w = r->win_w;
    r->page_h = r->win_h - kChromeHeight;
}

static TTF_Font* get_font(KitaeRenderer* r, int size_px, int weight, int italic, int monospace) {
    std::string key = std::to_string(size_px) + ":" + std::to_string(weight) + ":" +
                      std::to_string(italic) + ":" + std::to_string(monospace);
    auto it = r->font_cache.find(key);
    if (it != r->font_cache.end()) return it->second;
    TTF_Font* f = OpenFont(size_px, weight, italic, monospace);
    if (f) r->font_cache[key] = f;
    return f;
}

static GlyphTexture rasterize(KitaeRenderer* r, const std::string& text, int size_px, int weight,
                              int italic, int monospace, SDL_Color color) {
    TextKey key{text, size_px, weight, italic, monospace};
    auto it = r->text_cache.find(key);
    if (it != r->text_cache.end()) return it->second;

    GlyphTexture gt;
    TTF_Font* f = get_font(r, size_px, weight, italic, monospace);
    if (f) {
        SDL_Surface* surf = TTF_RenderUTF8_Blended(f, text.c_str(), color);
        if (surf) {
            gt.tex = SDL_CreateTextureFromSurface(r->ren, surf);
            gt.w = surf->w;
            gt.h = surf->h;
            SDL_FreeSurface(surf);
            if (gt.tex) SDL_SetTextureBlendMode(gt.tex, SDL_BLENDMODE_BLEND);
        }
    }
    if (r->text_cache.size() > 4096) {
        for (auto& kv : r->text_cache) {
            if (kv.second.tex) SDL_DestroyTexture(kv.second.tex);
        }
        r->text_cache.clear();
    }
    r->text_cache[key] = gt;
    return gt;
}

static void draw_text_pts(KitaeRenderer* r, const std::string& text, float x, float y_baseline,
                          int size_px, int weight, int italic, int monospace, SDL_Color color) {
    if (text.empty()) return;
    GlyphTexture gt = rasterize(r, text, size_px, weight, italic, monospace, color);
    if (!gt.tex) return;
    // Anchor at baseline: texture top = baseline - ascent of the rasterized font.
    TTF_Font* f = get_font(r, size_px, weight, italic, monospace);
    float ascent = f ? (float)TTF_FontAscent(f) : (float)size_px * 0.8f;
    SDL_FRect dst{x, y_baseline - ascent / r->dpi, (float)gt.w / r->dpi, (float)gt.h / r->dpi};
    SDL_RenderCopyF(r->ren, gt.tex, nullptr, &dst);
}

// Length in bytes of the UTF-8 sequence starting with byte `b` (1-4).
static int utf8_seq_len(unsigned char b) {
    if (b < 0x80) return 1;
    if ((b & 0xE0) == 0xC0) return 2;
    if ((b & 0xF0) == 0xE0) return 3;
    if ((b & 0xF8) == 0xF0) return 4;
    return 1;
}

// Letter-spaced run (Q3): draw per character at measure(char) + spacing
// advances — the exact sum layout measured the run with. Kerning is lost on
// spaced runs (accepted; matches the engine's measure_run_sp formula).
static void draw_text_spaced(KitaeRenderer* r, const std::string& text, float x, float y_baseline,
                             int size_px, int weight, int italic, int monospace, SDL_Color color,
                             float spacing) {
    size_t i = 0;
    float cx = x;
    while (i < text.size()) {
        int len = utf8_seq_len((unsigned char)text[i]);
        len = (int)std::min((size_t)len, text.size() - i);
        std::string one = text.substr(i, (size_t)len);
        draw_text_pts(r, one, cx, y_baseline, size_px, weight, italic, monospace, color);
        // Advance: this glyph's width + spacing, except after the last glyph
        // (CSS gives no trailing spacing; matches measure_run_sp exactly).
        float w = kitae_renderer_measure_text(r, one.c_str(), (float)size_px / r->dpi,
                                              (uint32_t)weight, italic, monospace);
        bool last = i + (size_t)len >= text.size();
        if (!last) cx += w + spacing;
        i += (size_t)len;
    }
}

// --- Page painting ---------------------------------------------------------

// FNV-1a over the RGBA bytes; distinguishes a re-stepped video frame under
// the same payload id from an unchanged one.
static uint64_t hash_pixels(const uint8_t* data, size_t len) {
    uint64_t h = 1469598103934665603ull;
    for (size_t i = 0; i < len; i++) h = (h ^ data[i]) * 1099511628211ull;
    return h;
}

// Upload (or reuse) the texture for this payload. A matching pixel hash means
// the raster content is unchanged; anything else re-uploads.
static SDL_Texture* get_image_texture(KitaeRenderer* r, const KitaeDLImage& im) {
    ImageTexture& entry = r->image_cache[im.payload];
    uint64_t hash =
        hash_pixels(im.pixels, (size_t)im.width * im.height * 4) ^ ((uint64_t)im.width << 32);
    if (entry.tex && entry.hash == hash) return entry.tex;
    if (entry.tex) SDL_DestroyTexture(entry.tex);
    entry.tex = nullptr;
    SDL_Surface* surf = SDL_CreateRGBSurfaceWithFormatFrom(
        (void*)im.pixels, (int)im.width, (int)im.height, 32, im.width * 4,
        SDL_PIXELFORMAT_RGBA32);
    if (surf) {
        entry.tex = SDL_CreateTextureFromSurface(r->ren, surf);
        SDL_FreeSurface(surf);
    }
    entry.hash = hash;
    entry.w = im.width;
    entry.h = im.height;
    if (entry.tex) SDL_SetTextureBlendMode(entry.tex, SDL_BLENDMODE_BLEND);
    if (r->image_cache.size() > 512) {
        for (auto& kv : r->image_cache) {
            if (kv.second.tex) SDL_DestroyTexture(kv.second.tex);
        }
        r->image_cache.clear();
        return nullptr;
    }
    return entry.tex;
}

// object-fit: box the decoded raster into the item per mode (cover clips in
// the caller). Returns the drawn box (dx, dy, dw, dh) in page space.
static void image_fit_box(const KitaeDLImage& im, float* dx, float* dy, float* dw, float* dh) {
    float sx = (float)im.width, shy = (float)im.height;
    float scale;
    switch (im.fit) {
        case 0: scale = 0.0f; break; // fill stretches
        case 2:
        case 3: scale = std::max(im.w / sx, im.h / shy); break; // cover
        case 4: scale = std::min(1.0f, std::min(im.w / sx, im.h / shy)); break;
        default: scale = std::min(im.w / sx, im.h / shy); break; // contain/none
    }
    *dw = (im.fit == 0) ? im.w : sx * scale;
    *dh = (im.fit == 0) ? im.h : shy * scale;
    *dx = im.x + (im.w - *dw) / 2.0f;
    *dy = im.y + (im.h - *dh) / 2.0f;
}

static void draw_image(KitaeRenderer* r, const KitaeDLImage& im, float sy, float view_bottom) {
    if (im.y + im.h < sy || im.y > view_bottom) return;
    if (im.width == 0 || im.height == 0 || !im.pixels) return;
    SDL_Texture* tex = get_image_texture(r, im);
    if (!tex) return;

    float dx, dy, dw, dh;
    image_fit_box(im, &dx, &dy, &dw, &dh);
    if (im.fit == 2) {
        SDL_Rect clip{(int)im.x, (int)(kChromeHeight + im.y - sy), (int)im.w, (int)im.h};
        SDL_RenderSetClipRect(r->ren, &clip);
    }
    SDL_FRect dst{dx, kChromeHeight + (dy - sy), dw, dh};
    SDL_RenderCopyF(r->ren, tex, nullptr, &dst);
    if (im.fit == 2) SDL_RenderSetClipRect(r->ren, nullptr);
}

// --- Transformed items (Q2) ------------------------------------------------
//
// Layout bakes a 2×3 matrix onto a sparse set of rects, texts, images and
// shadows. The stored geometry stays pre-transform page space; painting maps
// every corner through the matrix and fills the result as a quad
// (SDL_RenderGeometry). Scanline effects — rounded corners, per-pixel
// gradients, cover clipping — don't survive an arbitrary affine map, so
// transformed items paint whole and flat: a rect uses `color` (a gradient's
// first stop / image fallback). Documented v1 approximation; the GPU and
// Swift paths keep the same convention so the three backends stay in parity.
// Blend mode is the caller's: textured quads use the texture's blend mode,
// solid quads match the untransformed fill behavior, and the transformed
// shadow branch enables blending for its alpha layers itself.

// Fill the page-space box (x, y, w, h) mapped through `m` as a solid quad.
static void draw_quad(KitaeRenderer* r, const float* m, float x, float y, float w, float h,
                      float sy, float view_bottom, uint32_t color) {
    SDL_Vertex v[4];
    SDL_Color c = ToSdlColor(color);
    const float px[4] = {x, x + w, x + w, x};
    const float py[4] = {y, y, y + h, y + h};
    float mny = 1e9f, mxy = -1e9f;
    for (int i = 0; i < 4; i++) {
        float ox = m[0] * px[i] + m[1] * py[i] + m[2];
        float oy = m[3] * px[i] + m[4] * py[i] + m[5];
        v[i].position.x = ox;
        v[i].position.y = kChromeHeight + (oy - sy);
        v[i].color = c;
        v[i].tex_coord.x = 0.0f;
        v[i].tex_coord.y = 0.0f;
        mny = std::min(mny, v[i].position.y);
        mxy = std::max(mxy, v[i].position.y);
    }
    if (mxy < (float)kChromeHeight || mny > (float)kChromeHeight + (view_bottom - sy)) return;
    const int idx[6] = {0, 1, 2, 0, 2, 3};
    SDL_RenderGeometry(r->ren, nullptr, v, 4, idx, 6);
}

// Draw the texture mapped through `m` over the page-space box (x, y, w, h).
static void draw_tex_quad(KitaeRenderer* r, SDL_Texture* tex, const float* m, float x, float y,
                          float w, float h, float sy, float view_bottom) {
    SDL_Vertex v[4];
    SDL_Color white = {255, 255, 255, 255}; // vertex color × texture = texture
    const float px[4] = {x, x + w, x + w, x};
    const float py[4] = {y, y, y + h, y + h};
    const float tu[4] = {0.0f, 1.0f, 1.0f, 0.0f};
    const float tv[4] = {0.0f, 0.0f, 1.0f, 1.0f};
    float mny = 1e9f, mxy = -1e9f;
    for (int i = 0; i < 4; i++) {
        float ox = m[0] * px[i] + m[1] * py[i] + m[2];
        float oy = m[3] * px[i] + m[4] * py[i] + m[5];
        v[i].position.x = ox;
        v[i].position.y = kChromeHeight + (oy - sy);
        v[i].color = white;
        v[i].tex_coord.x = tu[i];
        v[i].tex_coord.y = tv[i];
        mny = std::min(mny, v[i].position.y);
        mxy = std::max(mxy, v[i].position.y);
    }
    if (mxy < (float)kChromeHeight || mny > (float)kChromeHeight + (view_bottom - sy)) return;
    const int idx[6] = {0, 1, 2, 0, 2, 3};
    SDL_RenderGeometry(r->ren, tex, v, 4, idx, 6);
}

// Text run mapped through the item matrix: rasterize (per glyph when
// letter-spaced), advance in untransformed page space, map each texture.
static void draw_text_matrix(KitaeRenderer* r, const KitaeDLText& t, float sy,
                             float view_bottom) {
    int size_px = std::max(6, (int)lroundf(t.size * r->dpi));
    TTF_Font* f = get_font(r, size_px, (int)t.weight, t.italic, t.monospace);
    float ascent = f ? (float)TTF_FontAscent(f) / r->dpi : t.size * 0.8f;
    if (t.letter_spacing != 0.0f) {
        const std::string text(t.text);
        size_t i = 0;
        float cx = t.x;
        while (i < text.size()) {
            int len = utf8_seq_len((unsigned char)text[i]);
            len = (int)std::min((size_t)len, text.size() - i);
            std::string one = text.substr(i, (size_t)len);
            GlyphTexture gt = rasterize(r, one, size_px, (int)t.weight, t.italic, t.monospace,
                                        ToSdlColor(t.color));
            if (gt.tex) {
                draw_tex_quad(r, gt.tex, t.m, cx, t.y - ascent, (float)gt.w / r->dpi,
                              (float)gt.h / r->dpi, sy, view_bottom);
            }
            float w = kitae_renderer_measure_text(r, one.c_str(), (float)size_px / r->dpi,
                                                  t.weight, t.italic, t.monospace);
            bool last = i + (size_t)len >= text.size();
            if (!last) cx += w + t.letter_spacing;
            i += (size_t)len;
        }
        return;
    }
    GlyphTexture gt = rasterize(r, std::string(t.text), size_px, (int)t.weight, t.italic,
                                t.monospace, ToSdlColor(t.color));
    if (!gt.tex) return;
    draw_tex_quad(r, gt.tex, t.m, t.x, t.y - ascent, (float)gt.w / r->dpi, (float)gt.h / r->dpi,
                  sy, view_bottom);
}

static void paint_page(KitaeRenderer* r) {
    SDL_Rect clip{0, kChromeHeight, r->win_w, std::max(0, r->page_h)};
    SDL_RenderSetClipRect(r->ren, &clip);
    const float sy = r->scroll_y;
    const float view_bottom = sy + (float)r->page_h;

    for (const auto& sh : r->shadows) {
        if (sh.inset) continue; // SDL shell approximates outset only
        if (sh.transformed) {
            // Layered alpha shells mapped through the item matrix; blending is
            // enabled for the block (draw_quad leaves the mode alone).
            SDL_SetRenderDrawBlendMode(r->ren, SDL_BLENDMODE_BLEND);
            float layers = std::min(16.0f, std::max(1.0f, ceilf(sh.blur)));
            for (int i = (int)layers; i >= 1; i--) {
                float k = (float)i / layers;
                float expand = sh.spread + sh.blur * k;
                uint32_t c = sh.color;
                float a = ((c & 0xFF) / 255.0f) * (1.0f - k) * (1.0f / layers) * 2.0f;
                if (a <= 0.004f) continue;
                uint32_t faded = (c & 0xFFFFFF00) | (uint32_t)(a * 255.0f + 0.5f);
                draw_quad(r, sh.m, sh.x + sh.offset_x - expand, sh.y + sh.offset_y - expand,
                          sh.w + 2.0f * expand, sh.h + 2.0f * expand, sy, view_bottom, faded);
            }
            SDL_SetRenderDrawBlendMode(r->ren, SDL_BLENDMODE_NONE);
            continue;
        }
        if (sh.y + sh.h + sh.offset_y < sy || sh.y + sh.offset_y > view_bottom) continue;
        draw_shadow(r, sh, sy, view_bottom);
    }

    for (const auto& rc : r->rects) {
        if (rc.transformed) {
            draw_quad(r, rc.m, rc.x, rc.y, rc.w, rc.h, sy, view_bottom, rc.color);
            continue;
        }
        draw_rect(r, rc, sy, view_bottom);
    }

    for (const auto& im : r->images) {
        if (im.transformed) {
            if (im.width == 0 || im.height == 0 || !im.pixels) continue;
            SDL_Texture* tex = get_image_texture(r, im);
            if (!tex) continue;
            float dx, dy, dw, dh;
            image_fit_box(im, &dx, &dy, &dw, &dh);
            // Cover's source-space clip doesn't survive an affine map;
            // transformed images paint whole (documented v1 approximation).
            draw_tex_quad(r, tex, im.m, dx, dy, dw, dh, sy, view_bottom);
            continue;
        }
        draw_image(r, im, sy, view_bottom);
    }

    for (const auto& t : r->texts) {
        if (t.transformed) {
            draw_text_matrix(r, t, sy, view_bottom);
            continue;
        }
        if (t.y - t.size * 1.6f > view_bottom || t.y + t.size * 0.4f < sy) continue;
        int size_px = std::max(6, (int)lroundf(t.size * r->dpi));
        float baseline = kChromeHeight + (t.y - sy);
        if (t.letter_spacing != 0.0f) {
            draw_text_spaced(r, t.text, t.x, baseline, size_px, (int)t.weight, t.italic,
                             t.monospace, ToSdlColor(t.color), t.letter_spacing);
        } else {
            draw_text_pts(r, t.text, t.x, baseline, size_px, (int)t.weight, t.italic,
                          t.monospace, ToSdlColor(t.color));
        }
    }

    SDL_RenderSetClipRect(r->ren, nullptr);
}

static void paint_scrollbar(KitaeRenderer* r) {
    float view = (float)r->page_h;
    if (r->content_h <= view || view <= 0) return;
    float track_h = view - 16.0f;
    float thumb_h = std::max(36.0f, track_h * view / r->content_h);
    float max_scroll = r->content_h - view;
    float t = std::clamp(r->scroll_y / max_scroll, 0.0f, 1.0f);
    float y = 8.0f + t * (track_h - thumb_h);
    SDL_Color c = ToSdlColor(kScrollbar);
    SDL_SetRenderDrawColor(r->ren, c.r, c.g, c.b, c.a);
    SDL_FRect bar{(float)(r->win_w - 8), kChromeHeight + y, 6.0f, thumb_h};
    SDL_RenderFillRectF(r->ren, &bar);
}

// --- Chrome painting -------------------------------------------------------

static void draw_btn_icon(KitaeRenderer* r, int idx, float cx, float cy) {
    SDL_SetRenderDrawColor(r->ren, (kChromeFg >> 24) & 0xFF, (kChromeFg >> 16) & 0xFF,
                           (kChromeFg >> 8) & 0xFF, 0xFF);
    const float s = 5.0f;
    if (idx == 0) { // back: chevron + shaft
        SDL_RenderDrawLineF(r->ren, cx + 1, cy - s, cx - s, cy);
        SDL_RenderDrawLineF(r->ren, cx - s, cy, cx + 1, cy + s);
        SDL_RenderDrawLineF(r->ren, cx - s, cy, cx + s, cy);
    } else if (idx == 1) { // forward
        SDL_RenderDrawLineF(r->ren, cx - 1, cy - s, cx + s, cy);
        SDL_RenderDrawLineF(r->ren, cx + s, cy, cx - 1, cy + s);
        SDL_RenderDrawLineF(r->ren, cx - s, cy, cx + s, cy);
    } else { // reload: open circle + arrow head
        for (int a = 40; a < 360; a += 10) {
            float t1 = a * 3.14159f / 180.0f, t2 = (a + 9) * 3.14159f / 180.0f;
            SDL_RenderDrawLineF(r->ren, cx + cosf(t1) * s, cy + sinf(t1) * s, cx + cosf(t2) * s,
                                cy + sinf(t2) * s);
        }
        SDL_RenderDrawLineF(r->ren, cx + s, cy - s, cx + s, cy - s - 4);
        SDL_RenderDrawLineF(r->ren, cx + s, cy - s, cx + s - 4, cy - s);
    }
}

static void paint_chrome(KitaeRenderer* r) {
    SDL_SetRenderDrawColor(r->ren, 0x1B, 0x1E, 0x24, 0xFF);
    SDL_Rect bar{0, 0, r->win_w, kChromeHeight};
    SDL_RenderFillRect(r->ren, &bar);

    const int btn_r = 15;
    const int btn_y = kChromeHeight / 2;
    const int btn_x[3] = {20, 58, 96};
    for (int i = 0; i < 3; i++) {
        if (r->hover_btn == i) {
            SDL_SetRenderDrawColor(r->ren, 0x33, 0x3A, 0x47, 0xFF);
            SDL_Rect b{btn_x[i] - btn_r, btn_y - btn_r, btn_r * 2, btn_r * 2};
            SDL_RenderFillRect(r->ren, &b);
        }
        draw_btn_icon(r, i, (float)btn_x[i], (float)btn_y);
    }

    SDL_FRect addr{112, 10, (float)std::max(80, r->win_w - 124), 32};
    SDL_SetRenderDrawColor(r->ren, 0x2A, 0x2F, 0x3A, 0xFF);
    SDL_RenderFillRectF(r->ren, &addr);
    if (r->focus_addr) {
        SDL_SetRenderDrawColor(r->ren, (kFocusBorder >> 24) & 0xFF, (kFocusBorder >> 16) & 0xFF,
                               (kFocusBorder >> 8) & 0xFF, 0xFF);
        SDL_RenderDrawRectF(r->ren, &addr);
    }

    if (!r->addr_text.empty()) {
        int size_px = std::max(6, (int)lroundf(15.0f * r->dpi));
        TTF_Font* f = get_font(r, size_px, 400, 0, 0);
        if (f) {
            int tw = 0;
            TTF_SizeUTF8(f, r->addr_text.c_str(), &tw, nullptr);
            float avail = addr.w - 16.0f;
            float tx = addr.x + 8.0f;
            if ((float)tw / r->dpi > avail) tx = addr.x + 8.0f + (avail - (float)tw / r->dpi);
            SDL_Rect clip{(int)addr.x, (int)addr.y, (int)addr.w, (int)addr.h};
            SDL_RenderSetClipRect(r->ren, &clip);
            // Draw with our cached-texture path for consistency (color = bar fg).
            SDL_Color c = ToSdlColor(r->focus_addr ? kChromeFg : kAddrFg);
            // Baseline: vertically center 15pt text in the 32pt bar.
            float ascent_pts = (float)TTF_FontAscent(f) / r->dpi;
            float baseline = addr.y + (addr.h - 15.0f) / 2.0f + ascent_pts;
            draw_text_pts(r, r->addr_text, tx, baseline, size_px, 400, 0, 0, c);
            if (r->focus_addr) {
                // Caret
                float caret_x = tx + (float)tw / r->dpi + 1.0f;
                if (SDL_GetTicks() % 1066 < 533) {
                    SDL_SetRenderDrawColor(r->ren, 0xE8, 0xEA, 0xF0, 0xFF);
                    SDL_RenderDrawLineF(r->ren, caret_x, addr.y + 7, caret_x, addr.y + addr.h - 7);
                }
            }
            SDL_RenderSetClipRect(r->ren, nullptr);
        }
    }
}

static void paint(KitaeRenderer* r) {
    SDL_SetRenderDrawColor(r->ren, 0xFF, 0xFF, 0xFF, 0xFF);
    SDL_RenderClear(r->ren);
    paint_page(r);
    paint_scrollbar(r);
    paint_chrome(r);
    SDL_RenderPresent(r->ren);
}

// --- Input -----------------------------------------------------------------

static int hit_chrome(KitaeRenderer* r, int x, int y) {
    const int btn_y = kChromeHeight / 2;
    const int btn_x[3] = {20, 58, 96};
    for (int i = 0; i < 3; i++) {
        int dx = x - btn_x[i], dy = y - btn_y;
        if (dx * dx + dy * dy <= 17 * 17) return 100 + i;
    }
    if (x >= 112 && x <= r->win_w - 12 && y >= 10 && y <= 42) return 200;
    return -1;
}

static void queue_event(KitaeRenderer* r, KitaeEvent e) { r->pending.push_back(e); }

static void handle_mouse_down(KitaeRenderer* r, int x, int y) {
    int chrome = hit_chrome(r, x, y);
    if (chrome == 200) {
        r->focus_addr = true;
        return;
    }
    r->focus_addr = false;
    if (chrome >= 100) {
        KitaeEvent e{};
        int btn = chrome - 100;
        e.kind = (btn == 0) ? KITAE_EVENT_BACK : (btn == 1) ? KITAE_EVENT_FORWARD
                                                            : KITAE_EVENT_RELOAD;
        queue_event(r, e);
        return;
    }
    if (y > kChromeHeight) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_CLICK;
        e.a = (float)x;
        e.b = (float)(y - kChromeHeight) + r->scroll_y; // page coords
        queue_event(r, e);
    }
}

static void handle_mouse_move(KitaeRenderer* r, int x, int y) {
    int chrome = hit_chrome(r, x, y);
    r->hover_btn = (chrome >= 100 && chrome < 200) ? chrome - 100 : -1;
    if (chrome == -1 && y > kChromeHeight) {
        bool link = kitae_app_hit_link((float)x, (float)(y - kChromeHeight) + r->scroll_y);
        SDL_SetCursor(SDL_CreateSystemCursor(link ? SDL_SYSTEM_CURSOR_HAND
                                                  : SDL_SYSTEM_CURSOR_ARROW));
    } else {
        SDL_SetCursor(SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_ARROW));
    }
}

static void handle_key(KitaeRenderer* r, SDL_KeyboardEvent& k) {
    SDL_Keycode sym = k.keysym.sym;
    Uint16 mod = k.keysym.mod;
    bool cmd = (mod & KMOD_GUI) || (mod & KMOD_CTRL);

    if (r->focus_addr) {
        if (sym == SDLK_RETURN || sym == SDLK_KP_ENTER) {
            if (!r->addr_text.empty()) {
                KitaeEvent e{};
                e.kind = KITAE_EVENT_NAVIGATE;
                snprintf(e.url, sizeof(e.url), "%s", r->addr_text.c_str());
                queue_event(r, e);
            }
            r->focus_addr = false;
        } else if (sym == SDLK_ESCAPE) {
            r->focus_addr = false;
        } else if (sym == SDLK_BACKSPACE) {
            if (!r->addr_text.empty()) {
                size_t len = r->addr_text.size();
                while (len > 0 && ((unsigned char)r->addr_text[len - 1] & 0xC0) == 0x80) len--;
                if (len > 0) len--;
                r->addr_text.resize(len);
            }
        } else if ((sym == SDLK_v) && cmd) {
            char* clip = SDL_GetClipboardText();
            if (clip) {
                r->addr_text += clip;
                SDL_free(clip);
            }
        } else if ((sym == SDLK_l) && cmd) {
            // keep focus
        }
        return;
    }

    // Global shortcuts / page scrolling
    if (cmd && sym == SDLK_l) {
        r->focus_addr = true;
    } else if (cmd && sym == SDLK_r) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_RELOAD;
        queue_event(r, e);
    } else if ((cmd && sym == SDLK_LEFT) || (mod & KMOD_ALT && sym == SDLK_LEFT)) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_BACK;
        queue_event(r, e);
    } else if ((cmd && sym == SDLK_RIGHT) || (mod & KMOD_ALT && sym == SDLK_RIGHT)) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_FORWARD;
        queue_event(r, e);
    } else if (sym == SDLK_DOWN) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_SCROLL;
        e.a = 60.0f;
        queue_event(r, e);
    } else if (sym == SDLK_UP) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_SCROLL;
        e.a = -60.0f;
        queue_event(r, e);
    } else if (sym == SDLK_SPACE || sym == SDLK_PAGEDOWN) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_SCROLL;
        e.a = (float)r->page_h * 0.85f;
        queue_event(r, e);
    } else if (sym == SDLK_PAGEUP) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_SCROLL;
        e.a = -(float)r->page_h * 0.85f;
        queue_event(r, e);
    } else if (sym == SDLK_HOME) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_SCROLL;
        e.a = -1e9f;
        queue_event(r, e);
    } else if (sym == SDLK_END) {
        KitaeEvent e{};
        e.kind = KITAE_EVENT_SCROLL;
        e.a = 1e9f;
        queue_event(r, e);
    }
}

static void handle_text_input(KitaeRenderer* r, const char* t) {
    if (r->focus_addr) r->addr_text += t;
}

extern "C" {

KitaeRenderer* kitae_renderer_init(int width, int height, const char* title) {
    if (SDL_Init(SDL_INIT_VIDEO | SDL_INIT_EVENTS) < 0) {
        fprintf(stderr, "[kitae] SDL init failed: %s\n", SDL_GetError());
        return nullptr;
    }
    if (TTF_Init() == -1) {
        fprintf(stderr, "[kitae] SDL_ttf init failed: %s\n", TTF_GetError());
        SDL_Quit();
        return nullptr;
    }
    KitaeRenderer* r = new KitaeRenderer();
    r->win_w = width;
    r->win_h = height;
    SDL_SetHint(SDL_HINT_RENDER_SCALE_QUALITY, "1");
    r->window =
        SDL_CreateWindow(title, SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED, width, height,
                         SDL_WINDOW_SHOWN | SDL_WINDOW_RESIZABLE | SDL_WINDOW_ALLOW_HIGHDPI);
    if (!r->window) {
        fprintf(stderr, "[kitae] window creation failed: %s\n", SDL_GetError());
        delete r;
        TTF_Quit();
        SDL_Quit();
        return nullptr;
    }
    r->ren = SDL_CreateRenderer(r->window, -1,
                                SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    if (!r->ren) {
        r->ren = SDL_CreateRenderer(r->window, -1, SDL_RENDERER_SOFTWARE);
    }
    if (!r->ren) {
        fprintf(stderr, "[kitae] renderer creation failed: %s\n", SDL_GetError());
        SDL_DestroyWindow(r->window);
        delete r;
        TTF_Quit();
        SDL_Quit();
        return nullptr;
    }
    compute_page_viewport(r);
    SDL_SetRenderDrawBlendMode(r->ren, SDL_BLENDMODE_BLEND);
    SDL_StartTextInput();
    return r;
}

void kitae_renderer_set_title(KitaeRenderer* r, const char* title) {
    SDL_SetWindowTitle(r->window, title);
}

void kitae_renderer_set_address(KitaeRenderer* r, const char* url) {
    if (url) r->addr_text = url;
}

void kitae_renderer_viewport(const KitaeRenderer* r, int* w, int* h) {
    if (w) *w = r->page_w;
    if (h) *h = r->page_h;
}

void kitae_renderer_clear_list(KitaeRenderer* r) {
    r->rects.clear();
    r->texts.clear();
    r->text_storage.clear();
    r->images.clear();
    r->image_storage.clear();
    r->shadows.clear();
}

void kitae_renderer_push_rect(KitaeRenderer* r, const KitaeDLRect* rect) {
    r->rects.push_back(*rect);
}

void kitae_renderer_push_text(KitaeRenderer* r, const KitaeDLText* text) {
    // Copy the bytes; the caller's buffer is a temporary CString.
    r->text_storage.emplace_back(text->text);
    KitaeDLText copy = *text;
    copy.text = r->text_storage.back().c_str();
    r->texts.push_back(copy);
}

void kitae_renderer_push_shadow(KitaeRenderer* r, const KitaeDLShadow* shadow) {
    if (shadow) r->shadows.push_back(*shadow);
}

void kitae_renderer_push_image(KitaeRenderer* r, const KitaeDLImage* img) {
    if (!img || !img->pixels || img->width == 0 || img->height == 0) return;
    size_t len = (size_t)img->width * img->height * 4;
    r->image_storage.emplace_back(img->pixels, img->pixels + len);
    KitaeDLImage copy = *img;
    copy.pixels = r->image_storage.back().data();
    r->images.push_back(copy);
}

void kitae_renderer_set_scroll(KitaeRenderer* r, float scroll_y, float content_height) {
    r->scroll_y = scroll_y;
    r->content_h = content_height > 1.0f ? content_height : 1.0f;
}

float kitae_renderer_measure_text(const KitaeRenderer* r, const char* text, float size,
                                  uint32_t weight, int italic, int monospace) {
    if (!text || !*text) return 0.0f;
    int size_px = std::max(6, (int)lroundf(size * r->dpi));
    TTF_Font* f = get_font((KitaeRenderer*)r, size_px, (int)weight, italic, monospace);
    if (!f) return (float)strlen(text) * size * 0.5f;
    int w = 0;
    TTF_SizeUTF8(f, text, &w, nullptr);
    return (float)w / r->dpi;
}

void kitae_renderer_font_metrics(const KitaeRenderer* r, float size, int monospace,
                                 float* ascent, float* descent, float* line_gap) {
    int size_px = std::max(6, (int)lroundf(size * r->dpi));
    TTF_Font* f = get_font((KitaeRenderer*)r, size_px, 400, 0, monospace);
    if (!f) {
        if (ascent) *ascent = size * 0.8f;
        if (descent) *descent = size * 0.2f;
        if (line_gap) *line_gap = size * 0.2f;
        return;
    }
    if (ascent) *ascent = (float)TTF_FontAscent(f) / r->dpi;
    if (descent) *descent = (float)(-TTF_FontDescent(f)) / r->dpi;
    if (line_gap) *line_gap = (float)TTF_FontLineSkip(f) / r->dpi;
}

int kitae_renderer_poll_event(KitaeRenderer* r, KitaeEvent* out) {
    if (!r->pending.empty()) {
        *out = r->pending.front();
        r->pending.erase(r->pending.begin());
        return 1;
    }

    SDL_Event e;
    while (SDL_PollEvent(&e)) {
        switch (e.type) {
            case SDL_QUIT:
                out->kind = KITAE_EVENT_QUIT;
                return 1;
            case SDL_WINDOWEVENT:
                if (e.window.event == SDL_WINDOWEVENT_SIZE_CHANGED) {
                    r->win_w = std::max(320, e.window.data1);
                    r->win_h = std::max(240, e.window.data2);
                    compute_page_viewport(r);
                    out->kind = KITAE_EVENT_RESIZE;
                    out->a = (float)r->page_w;
                    out->b = (float)r->page_h;
                    return 1;
                }
                break;
            case SDL_TEXTINPUT:
                handle_text_input(r, e.text.text);
                break;
            case SDL_KEYDOWN:
                if (e.key.keysym.sym == SDLK_ESCAPE && !r->focus_addr &&
                    !(e.key.keysym.mod & (KMOD_GUI | KMOD_CTRL))) {
                    out->kind = KITAE_EVENT_QUIT;
                    return 1;
                }
                handle_key(r, e.key);
                break;
            case SDL_MOUSEMOTION:
                handle_mouse_move(r, e.button.x, e.button.y);
                break;
            case SDL_MOUSEBUTTONDOWN:
                if (e.button.button == SDL_BUTTON_LEFT) handle_mouse_down(r, e.button.x, e.button.y);
                break;
            case SDL_MOUSEWHEEL: {
                int mx = 0, my = 0;
                SDL_GetMouseState(&mx, &my);
                if (my > kChromeHeight) {
                    out->kind = KITAE_EVENT_SCROLL;
                    float lines = (e.wheel.direction == SDL_MOUSEWHEEL_FLIPPED) ? -(float)e.wheel.y
                                                                                : (float)e.wheel.y;
                    out->a = -lines * 40.0f;
                    return 1;
                }
                break;
            }
            default:
                break;
        }
    }

    paint(r);
    return 0;
}

void kitae_renderer_destroy(KitaeRenderer* r) {
    if (!r) return;
    for (auto& kv : r->text_cache) {
        if (kv.second.tex) SDL_DestroyTexture(kv.second.tex);
    }
    for (auto& kv : r->image_cache) {
        if (kv.second.tex) SDL_DestroyTexture(kv.second.tex);
    }
    for (auto& kv : r->font_cache) {
        if (kv.second) TTF_CloseFont(kv.second);
    }
    if (r->ren) SDL_DestroyRenderer(r->ren);
    if (r->window) SDL_DestroyWindow(r->window);
    delete r;
    TTF_Quit();
    SDL_Quit();
}

} // extern "C"
