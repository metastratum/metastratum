# Build environment matching the lab hosts (Debian 13), so hm-agent links
# against the same glibc and libvirt.
FROM rust:1-trixie
RUN apt-get update \
 && apt-get install -y --no-install-recommends libvirt-dev protobuf-compiler pkg-config \
 && rm -rf /var/lib/apt/lists/*
# rust-toolchain.toml asks for `stable`; install it into the image instead of
# into every throwaway build container.
RUN rustup toolchain install stable --profile minimal -c rustfmt -c clippy && chmod -R a+w "$RUSTUP_HOME"
