# Static musl build so the runtime image can be FROM scratch.
#
# This works because of two properties of the dependency tree, both verified
# with `cargo tree`:
#   * TLS roots come from webpki-roots, compiled into the binary, so no
#     ca-certificates package is needed at runtime. This is normally THE
#     blocker for scratch images.
#   * The rustls crypto provider is `ring`, not `aws-lc-rs`, and ring builds
#     against musl cleanly.
#
# The target triple is derived from the builder's own architecture rather than
# hardcoded, so this builds natively on fly's x86_64 remote builder and on an
# arm64 Mac (Apple `container`) without cross-compilation.

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "$(uname -m)-unknown-linux-musl"

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cache dependency compilation separately from the application build.
RUN cargo chef cook --release --target "$(uname -m)-unknown-linux-musl" --recipe-path recipe.json
COPY . .
RUN TRIPLE="$(uname -m)-unknown-linux-musl" \
    && cargo build --release --target "$TRIPLE" --bin sb-watcher \
    && cp "target/$TRIPLE/release/sb-watcher" /app/sb-watcher \
    && strip /app/sb-watcher

# A passwd entry so the binary can run as a non-root user, and so that /etc
# exists in the image for the container runtime to mount /etc/resolv.conf into.
# musl's static resolver reads that file directly, which is how DNS works here
# without any NSS libraries.
RUN printf 'nobody:x:65534:65534:nobody:/:/sbin/nologin\n' > /app/passwd.min

FROM scratch AS runtime
COPY --from=builder /app/passwd.min /etc/passwd
COPY --from=builder /app/sb-watcher /sb-watcher
USER 65534:65534
ENTRYPOINT ["/sb-watcher"]
