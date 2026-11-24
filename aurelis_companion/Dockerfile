FROM rust:1.96.1-slim-bookworm AS build
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_BUILD_JOBS=2
WORKDIR /src
COPY rust/ ./rust/
WORKDIR /src/rust
RUN cargo build --locked --release -p aurelis-companion \
 && strip target/release/aurelis-companion

FROM debian:bookworm-20261104-slim
RUN apt-get update && apt-get install -y --no-install-recommends iputils-ping \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/rust/target/release/aurelis-companion /usr/local/bin/aurelis-companion
# Runs as root: the add-on's /data belongs to root. The binary is PID 1 (config.yaml has init: false).
EXPOSE 8765/tcp
ENTRYPOINT ["/usr/local/bin/aurelis-companion"]
