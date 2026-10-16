# kitae-server — the headless agent API (Track B, tier B3).
#
# Two stages: a full Rust image builds a **static musl binary**, and the
# runtime image carries only that binary plus the fonts text needs.
#
# Two things the runtime genuinely needs, and why:
# - **fonts**: the rasterizer measures and paints with the system's font
#   files. A container with no fonts still serves `read`/`extract`/`links`
#   (they never touch a glyph), and reports the missing fonts for
#   `screenshot`/`pdf` instead of returning a blank page.
# - **CA certificates**: page fetches over https verify against the
#   webpki roots compiled into the binary, so no OS trust store is
#   required — but `apk` and any curl-based health check want them.
#
# Build:  docker build -t kitae-server .
# Run:    docker run --rm -p 8787:8787 kitae-server
#         (the default posture is read-only; pass `--allow '*'` to let a
#          client drive the browser, e.g. `docker run … kitae-server
#          --bind 0.0.0.0:8787 --allow '*'`)
#
# The binary is static, so the runtime stage could be `scratch` plus a
# COPYed font directory. Alpine is used instead because `ttf-dejavu` and
# `fontconfig` are one line there, and a `scratch` image whose fonts are
# missing fails in a way that is harder to debug than a 12 MB base.

FROM rust:1-alpine AS build
WORKDIR /src
RUN apk add --no-cache musl-dev
# Static: no libc at runtime. (`ring` builds its own C and asm for musl.)
ENV RUSTFLAGS="-C target-feature=+crt-static"
# The whole workspace is copied because the agent plane links most of it;
# a finer-grained COPY would need a dependency manifest dance that rots.
COPY . .
RUN cargo build --release -p kitae_server \
    && strip target/release/kitae-server \
    && cp target/release/kitae-server /usr/local/bin/kitae-server

FROM alpine:3
RUN apk add --no-cache ttf-dejavu fontconfig ca-certificates
COPY --from=build /usr/local/bin/kitae-server /usr/local/bin/kitae-server
# The container listens on all interfaces: reaching it from outside is the
# point. Put a TLS-terminating proxy in front of it for anything but a
# private network.
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/kitae-server"]
CMD ["--bind", "0.0.0.0:8787"]
