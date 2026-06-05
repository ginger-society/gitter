#!/bin/bash
set -e

BINARY="ginger-gitter-pipeline-hook"
TARGET="x86_64-unknown-linux-musl"

echo "[build-hook] Building $BINARY for $TARGET (Alpine/musl/amd64)..."

docker run --rm \
    --platform linux/amd64 \
    -v "$(pwd)":/workspace \
    -w /workspace \
    gingersociety/rust-cli-builder:latest-amd64 \
    bash -c "
        apt-get install -y musl-tools musl-dev make perl && \
        ln -s /usr/include/x86_64-linux-gnu/asm /usr/include/x86_64-linux-musl/asm && \
        ln -s /usr/include/generic /usr/include/x86_64-linux-musl/generic && \
        OPENSSL_STATIC=1 \
        OPENSSL_DIR=/usr/local/musl \
        cargo build --release --target $TARGET --bin $BINARY && \
        cp target/$TARGET/release/$BINARY target/release/$BINARY
    "

echo "[build-hook] Done: target/release/$BINARY"