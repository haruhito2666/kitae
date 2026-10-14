#ifndef KITAE_RENDER_GPU_H
#define KITAE_RENDER_GPU_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct KruGpuHandle KruGpuHandle;

KruGpuHandle *kru_gpu_init(void);
int kru_gpu_available(const KruGpuHandle *handle);
uint8_t *kru_gpu_render(const KruGpuHandle *handle, const uint8_t *frame,
                        size_t frame_len, uint32_t w, uint32_t h,
                        float scale, float scroll_x, float scroll_y);
void kru_gpu_pixels_free(uint8_t *ptr, size_t len);
void kru_gpu_clear_layers(const KruGpuHandle *handle);
int kru_gpu_set_lcd_text(const KruGpuHandle *handle, int mode);
void kru_gpu_shutdown(KruGpuHandle *handle);

#ifdef __cplusplus
}
#endif

#endif
