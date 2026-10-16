// Bridging header — exposes the Kitae engine and renderer C contracts to Swift.
#include "kitae_core.h"
#include "kitae_render_gpu.h"
// B4: the pure-Rust page rasterizer the agent's screenshot/pdf verbs use.
// The app's one call — kitae_headless_install_raster — hands the engine the
// vtable; nothing else in the headless contract is needed here.
#include "kitae_headless.h"
