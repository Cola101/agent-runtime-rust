FROM rust:1.88-bookworm AS build

ARG PACKAGE
ARG BINARY
WORKDIR /workspace
RUN apt-get update \
    && apt-get install --yes --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
COPY runtime ./runtime
COPY contracts ./contracts
WORKDIR /workspace/runtime
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/runtime/target \
    test -n "$PACKAGE" && test -n "$BINARY" \
    && cargo build --locked --release --package "$PACKAGE" --bin "$BINARY" \
    && cp "/workspace/runtime/target/release/$BINARY" /agent-runtime

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build --chown=65532:65532 /agent-runtime /usr/local/bin/agent-runtime
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/agent-runtime"]
