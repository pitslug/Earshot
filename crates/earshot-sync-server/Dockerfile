# The sync relay (T11.2, docs/sync-spec.md section 4). Build from the repository root:
#
#   docker build -f crates/earshot-sync-server/Dockerfile -t earshot-sync-server .
#
# Two stages: the pinned Rust toolchain builds the relay alone, and a slim Debian runs it as
# a user without a login, with its database on the /data volume.

FROM rust:1.99.1-slim-bookworm AS build
# The repository's rust-toolchain.toml also lists Windows targets; this variable makes rustup
# use the image's toolchain (the same version) and ignore that file.
ENV RUSTUP_TOOLCHAIN=1.99.1
WORKDIR /src
# Cargo needs every member's manifest to read the workspace; only the relay is built.
COPY . .
RUN cargo build --release --locked -p earshot-sync-server \
    && cp target/release/earshot-sync-server /earshot-sync-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin earshot \
    && mkdir /data && chown earshot /data
COPY --from=build /earshot-sync-server /usr/local/bin/earshot-sync-server
USER earshot
VOLUME /data
EXPOSE 8787
ENV EARSHOT_SYNC_DATA=/data EARSHOT_SYNC_LISTEN=0.0.0.0:8787
ENTRYPOINT ["/usr/local/bin/earshot-sync-server"]
