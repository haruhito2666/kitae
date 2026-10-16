# Kitae.app — the native macOS front end.
#
# Two build stages:
#   1. cargo build --release  → ../target/release/libkitae_core.a
#      (the pure-Rust engine: layout, net, pages; carries the vendored QTSQ
#      C objects, whose deps must resolve at final link — see QTSQ_DEPS)
#   2. swiftc Sources/*.swift → Kitae.app/Contents/MacOS/Kitae
#      (AppKit chrome + Core Text painting, statically linked against the
#      engine through the bridging header)
#
# `make` builds the bundle, `make run` launches it, `make clean` removes it.
#
# The app ships with the opt-in page-JS tier compiled in: PAGE_JS=OFF at
# build time drops back to the script-free engine.

PAGE_JS   ?= page-js
CARGO_FEATURES := live
ifneq ($(PAGE_JS),OFF)
	CARGO_FEATURES := live $(PAGE_JS)
endif

ROOT      := $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
REPO      := $(ROOT)/..
LIB       := $(REPO)/target/release/libkitae_core.a
GPU_LIB   := $(REPO)/target/release/libkitae_render_gpu.a
HEADLESS_LIB := $(REPO)/target/release/libkitae_headless.a
HEADER    := $(REPO)/crates/kitae_core/include/kitae_core.h
GPU_HEADER := $(REPO)/crates/kitae_render_gpu/include/kitae_render_gpu.h
HEADLESS_HEADER := $(REPO)/crates/kitae_headless/include/kitae_headless.h
APP       := Kitae.app
BIN       := $(APP)/Contents/MacOS/Kitae
SRCS      := $(wildcard Sources/*.swift)
ENGINE_SOURCES := $(shell find "$(REPO)/crates" "$(REPO)/native/qtsq" -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name '*.c' -o -name '*.h' \) -print 2>/dev/null)
ENGINE_STAMP := $(ROOT)/.build-tmp/engine.stamp
DEPLOYMENT_TARGET := 15.0
TARGET    := arm64-apple-macos$(DEPLOYMENT_TARGET)
export MACOSX_DEPLOYMENT_TARGET := $(DEPLOYMENT_TARGET)
CODESIGN_IDENTITY ?= -
# Mirror of crates/kitae_qtsq/build.rs: static archives never auto-pull
# their own dependencies, so the final binary links them. Opus comes from
# Homebrew (same path pkg-config resolves for the Rust build).
OPUS_DIR := $(shell pkg-config --variable=libdir opus 2>/dev/null || echo /opt/homebrew/Cellar/opus/1.6.1/lib)
QTSQ_DEPS := -lz -L$(OPUS_DIR) -lopus -lc++ -lobjc \
	-framework Security -framework CoreFoundation -framework CoreAudio \
	-framework AudioUnit -framework AudioToolbox -framework Metal \
	-framework MetalKit -framework Foundation -framework CoreBluetooth
PLATFORM_DEVELOPER := $(shell xcrun --show-sdk-platform-path)/Developer
NATIVE_TEST_BUNDLE := $(ROOT)/.build-tmp/NativeContractTests.xctest
NATIVE_TEST_BIN := $(NATIVE_TEST_BUNDLE)/Contents/MacOS/NativeContractTests
NATIVE_TEST_PLIST := $(ROOT)/Tests/Info.plist

.PHONY: all run clean native-test ffi-smoke

all: $(BIN)

native-test: $(LIB) $(GPU_LIB) $(HEADLESS_LIB) include/kitae_core.h include/kitae_render_gpu.h include/kitae_headless.h
	@mkdir -p $(NATIVE_TEST_BUNDLE)/Contents/MacOS
	@cp $(NATIVE_TEST_PLIST) $(NATIVE_TEST_BUNDLE)/Contents/Info.plist
	xcrun swiftc Sources/EngineFrame.swift Sources/DeepLink.swift Sources/AgentPanel.swift \
		Tests/NativeContractTests.swift \
		-target arm64-apple-macos$(DEPLOYMENT_TARGET) -swift-version 5 \
		-emit-library -emit-module -module-name NativeContractTests \
		-o $(NATIVE_TEST_BIN) \
		-import-objc-header BridgingHeader.h -I include \
		-F $(PLATFORM_DEVELOPER)/Library/Frameworks \
		-I $(PLATFORM_DEVELOPER)/usr/lib \
		-L $(PLATFORM_DEVELOPER)/usr/lib \
		-Xlinker -rpath -Xlinker $(PLATFORM_DEVELOPER)/usr/lib \
		-L $(REPO)/target/release \
		-Xlinker -force_load -Xlinker $(LIB) -lkitae_render_gpu -lkitae_headless \
		-lXCTestSwiftSupport \
		-framework AppKit -framework XCTest -framework SwiftUI \
		-framework AVFoundation -framework CoreText -framework Metal \
		-framework Security -framework CoreFoundation -framework CoreGraphics \
		-framework AudioToolbox -framework AudioUnit -framework CoreAudio \
		-framework MetalKit -framework CoreBluetooth -framework SystemConfiguration \
		-lc++ -lobjc -lz -liconv -L$(OPUS_DIR) -lopus
	xcrun xctest $(NATIVE_TEST_BUNDLE)

$(BIN): $(SRCS) BridgingHeader.h Info.plist $(LIB) $(GPU_LIB) $(HEADLESS_LIB) include/kitae_core.h include/kitae_render_gpu.h include/kitae_headless.h
	@mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	cp Info.plist $(APP)/Contents/Info.plist
	@if [ -d Resources ]; then cp -R Resources/* $(APP)/Contents/Resources/ 2>/dev/null || true; fi
	@# Both staticlibs bundle the full transitive dependency closure (std,
	@# kitae_style, the QTSQ C objects, ...). Drop every GPU-lib member that
	@# also lives in the core lib — they are byte-identical cargo artifacts,
	@# and the linker must see exactly one copy of each. Core is force-loaded
	@# so its closure is always present; the GPU lib then contributes only
	@# kitae_render_gpu's own objects.
	@mkdir -p $(ROOT)/.build-tmp
	@ar t $(LIB) | sort -u > $(ROOT)/.build-tmp/core_members.txt
	@ar t $(GPU_LIB) | sort -u > $(ROOT)/.build-tmp/gpu_members.txt
	@comm -12 $(ROOT)/.build-tmp/core_members.txt $(ROOT)/.build-tmp/gpu_members.txt \
		| grep -v '__.SYMDEF' > $(ROOT)/.build-tmp/dup_members.txt || true
	@if [ -s $(ROOT)/.build-tmp/dup_members.txt ]; then \
		xargs ar d $(GPU_LIB) < $(ROOT)/.build-tmp/dup_members.txt || true; \
	fi
	@# Same treatment for the rasterizer lib: it carries the closure of
	@# kitae_core and kitae_render_gpu (it paints with the CPU compositor),
	@# so drop every member the two other libs already provide and let it
	@# contribute only kitae_headless's own objects.
	@ar t $(HEADLESS_LIB) | sort -u > $(ROOT)/.build-tmp/headless_members.txt
	@cat $(ROOT)/.build-tmp/core_members.txt $(ROOT)/.build-tmp/gpu_members.txt \
		| sort -u > $(ROOT)/.build-tmp/other_members.txt
	@comm -12 $(ROOT)/.build-tmp/headless_members.txt $(ROOT)/.build-tmp/other_members.txt \
		| grep -v '__.SYMDEF' > $(ROOT)/.build-tmp/headless_dup.txt || true
	@if [ -s $(ROOT)/.build-tmp/headless_dup.txt ]; then \
		xargs ar d $(HEADLESS_LIB) < $(ROOT)/.build-tmp/headless_dup.txt || true; \
	fi
	swiftc $(SRCS) \
		-target $(TARGET) \
		-swift-version 5 \
		-O \
		-import-objc-header BridgingHeader.h \
		-I include \
		-L $(REPO)/target/release \
		-Xlinker -force_load -Xlinker $(LIB) \
		-lkitae_render_gpu -lkitae_headless \
		-framework AppKit -framework CoreText \
		-framework AVFoundation \
		-framework Metal \
		-framework Security -framework SystemConfiguration \
		-framework VideoToolbox -framework CoreMedia -framework CoreVideo \
		$(QTSQ_DEPS) \
		-liconv \
		-module-cache-path $(ROOT)/.module-cache \
		-Xcc -fmodules-cache-path=$(ROOT)/.module-cache \
		-o $(BIN)
	codesign --force --sign "$(CODESIGN_IDENTITY)" $(APP)
	touch $(APP)
	@echo "Built $(APP)"

$(ENGINE_STAMP): $(ENGINE_SOURCES) $(REPO)/Cargo.toml $(REPO)/Cargo.lock
	@mkdir -p $(ROOT)/.build-tmp
	cd $(REPO) && cargo build --release -p kitae_core --features "$(CARGO_FEATURES)" -p kitae_render_gpu -p kitae_headless
	@touch $(LIB) $(GPU_LIB) $(HEADLESS_LIB) $@

$(LIB) $(GPU_LIB) $(HEADLESS_LIB): $(ENGINE_STAMP)
	@:

include/kitae_core.h: $(HEADER)
	@mkdir -p include
	cp $< $@

include/kitae_render_gpu.h: $(GPU_HEADER)
	@mkdir -p include
	cp $< $@

include/kitae_headless.h: $(HEADLESS_HEADER)
	@mkdir -p include
	cp $< $@

ffi-smoke: $(LIB) $(GPU_LIB) $(HEADLESS_LIB) include/kitae_core.h include/kitae_headless.h
	$(CC) -I $(REPO)/crates/kitae_core/include -I $(REPO)/crates/kitae_headless/include \
		$(REPO)/tools/ffi_smoke.c \
		$(LIB) -L$(REPO)/target/release -lkitae_render_gpu -lkitae_headless -lz -liconv \
		-L$(OPUS_DIR) -lopus -lc++ -lobjc \
		-framework Security -framework CoreFoundation -framework CoreText \
		-framework CoreGraphics -framework CoreAudio -framework AudioUnit \
		-framework AudioToolbox -framework Metal -framework MetalKit \
		-framework Foundation -framework CoreBluetooth \
		-o $(ROOT)/.build-tmp/ffi-smoke
	$(ROOT)/.build-tmp/ffi-smoke

run: $(BIN)
	open $(APP)

clean:
	rm -rf $(APP) include $(ROOT)/.build-tmp $(ROOT)/.module-cache
