# syntax=docker/dockerfile:1.7

# Multi-stage build for ecal-mcp.
#
# Stage 1 (builder): installs the Rust toolchain and a published Eclipse eCAL
# .deb (matching the CI flow used by rustecal upstream), then compiles the
# release binaries.
#
# Stage 2 (runtime): minimal Ubuntu image with eCAL installed and the binaries
# copied in. Image runs `ecal-mcp` over stdio by default.

ARG ECAL_VERSION=v6.1.1
ARG RUST_VERSION=1.89
# Pin the base by tag; for fully reproducible builds override with a digest:
#   docker build --build-arg UBUNTU_REF='ubuntu:22.04@sha256:<digest>' .
ARG UBUNTU_REF=ubuntu:22.04

# ---------------------------------------------------------------------------
# Builder stage
# ---------------------------------------------------------------------------
FROM ${UBUNTU_REF} AS builder

ARG ECAL_VERSION
ARG RUST_VERSION

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=never \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl jq pkg-config build-essential \
        clang libclang-14-dev llvm-dev \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
    && rustc --version

# Pull the matching jammy_<arch>.deb from the eCAL GitHub release.
RUN set -eux; \
    ARCH=$(dpkg --print-architecture); \
    DEB_URL=$(curl -sSL "https://api.github.com/repos/eclipse-ecal/ecal/releases/tags/${ECAL_VERSION}" \
        | jq -r --arg arch "$ARCH" \
          '[.assets[] | select(.name|test("jammy_" + $arch + "\\.deb$"))][0].browser_download_url'); \
    test -n "$DEB_URL" -a "$DEB_URL" != "null"; \
    echo "Installing eCAL deb: $DEB_URL"; \
    curl -sSL -o /tmp/ecal.deb "$DEB_URL"; \
    apt-get update; \
    apt-get install -y --no-install-recommends /tmp/ecal.deb; \
    rm -f /tmp/ecal.deb; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency builds independently of source changes.
# `--locked` ensures builds use the committed Cargo.lock — no implicit upgrades.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > src/bin/test_publisher.rs \
    && echo 'fn main() {}' > src/bin/test_service_server.rs \
    && echo 'fn main() {}' > src/bin/test_subscriber.rs \
    && cargo build --release --locked --bins \
    && rm -rf src target/release/ecal-mcp \
              target/release/ecal-test-publisher \
              target/release/ecal-test-service-server \
              target/release/ecal-test-subscriber \
              target/release/deps/ecal_mcp-* \
              target/release/deps/ecal_test_publisher-* \
              target/release/deps/ecal_test_service_server-* \
              target/release/deps/ecal_test_subscriber-* \
              target/release/.fingerprint/ecal-mcp-* \
              target/release/.fingerprint/ecal-test-publisher-* \
              target/release/.fingerprint/ecal-test-service-server-* \
              target/release/.fingerprint/ecal-test-subscriber-*

COPY src ./src
RUN cargo build --release --locked --bins \
    && strip target/release/ecal-mcp \
             target/release/ecal-test-publisher \
             target/release/ecal-test-service-server \
             target/release/ecal-test-subscriber \
    && ls -la target/release/ecal-mcp target/release/ecal-test-publisher target/release/ecal-test-service-server target/release/ecal-test-subscriber

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM ${UBUNTU_REF} AS runtime

ARG ECAL_VERSION

ENV DEBIAN_FRONTEND=noninteractive

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl jq; \
    ARCH=$(dpkg --print-architecture); \
    DEB_URL=$(curl -sSL "https://api.github.com/repos/eclipse-ecal/ecal/releases/tags/${ECAL_VERSION}" \
        | jq -r --arg arch "$ARCH" \
          '[.assets[] | select(.name|test("jammy_" + $arch + "\\.deb$"))][0].browser_download_url'); \
    test -n "$DEB_URL" -a "$DEB_URL" != "null"; \
    curl -sSL -o /tmp/ecal.deb "$DEB_URL"; \
    apt-get install -y --no-install-recommends /tmp/ecal.deb; \
    rm -f /tmp/ecal.deb; \
    apt-get purge -y curl jq; \
    apt-get autoremove -y; \
    apt-get clean; \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/ecal-mcp                 /usr/local/bin/ecal-mcp
COPY --from=builder /build/target/release/ecal-test-publisher       /usr/local/bin/ecal-test-publisher
COPY --from=builder /build/target/release/ecal-test-service-server  /usr/local/bin/ecal-test-service-server
COPY --from=builder /build/target/release/ecal-test-subscriber      /usr/local/bin/ecal-test-subscriber

# Drop privileges. eCAL only needs access to /dev/shm (world-writable) and
# multicast/loopback, none of which require root.
RUN useradd --system --create-home --shell /usr/sbin/nologin ecal
USER ecal
WORKDIR /home/ecal

# eCAL writes runtime files into $HOME/.ecal — make sure that exists.
RUN mkdir -p /home/ecal/.ecal

ENV RUST_LOG=info,rmcp=warn

ENTRYPOINT ["/usr/local/bin/ecal-mcp"]
