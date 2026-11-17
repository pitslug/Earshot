# The summary worker (T11.6, docs/sync.md section 10). Build from the repository root:
#
#   docker build -f crates/earshot-worker/Dockerfile -t earshot-worker .
#
# Two stages, base images pinned by digest: the pinned Rust toolchain builds the worker (with the
# runtime's in-process engines left out, T11.6b: no whisper.cpp, ONNX Runtime or llama.cpp), and a
# slim Debian runs it as a user without a login, with its database and vault files on the /data
# volume.
# The build reads the built binary with `ldd` and copies exactly the shared libraries it needs
# that the slim image does not carry; the run stage checks with `ldd` that nothing is missing.
# Nothing secret is in any layer: the vault key and token are written by `join` at run time.

FROM rust:1.99.1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd AS build
# The repository's rust-toolchain.toml also lists Windows targets; this variable makes rustup
# use the image's toolchain (the same version) and ignore that file.
ENV RUSTUP_TOOLCHAIN=1.99.1
# Without the engines nothing runs CMake, a C++ compiler or bindgen: the only native code left is
# C (SQLite, bundled by rusqlite, and ring), which the image's own gcc builds.
WORKDIR /src
# Cargo needs every member's manifest to read the workspace; only the worker is built.
COPY . .
RUN cargo build --release --locked -p earshot-worker \
    && cp target/release/earshot-worker /earshot-worker
# The binary's shared libraries: every one ldd names, except glibc's (the slim image of the same
# Debian release carries those). A library ldd cannot find fails the build.
RUN set -eu; \
    export LD_LIBRARY_PATH="/src/target/release"; \
    ldd /earshot-worker | tee /ldd-build.txt; \
    if grep -q "not found" /ldd-build.txt; then echo "a shared library is missing"; exit 1; fi; \
    mkdir -p /libs; \
    awk '$2 == "=>" && $3 ~ /^\// { print $1 " " $3 }' /ldd-build.txt | while read -r name path; do \
        case "${name}" in \
            linux-vdso.so*|ld-linux*|libc.so*|libm.so*|libpthread.so*|libdl.so*|librt.so*) ;; \
            *) cp -L "${path}" "/libs/${name}" ;; \
        esac; \
    done; \
    ls -1 /libs

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin earshot \
    && mkdir /data && chown earshot /data && chmod 0700 /data
COPY --from=build /libs/ /usr/local/lib/earshot/
COPY --from=build /earshot-worker /usr/local/bin/earshot-worker
RUN echo /usr/local/lib/earshot > /etc/ld.so.conf.d/earshot.conf \
    && ldconfig \
    && ldd /usr/local/bin/earshot-worker | tee /tmp/ldd.txt \
    && if grep -q "not found" /tmp/ldd.txt; then echo "a shared library is missing"; exit 1; fi \
    && rm /tmp/ldd.txt \
    && /usr/local/bin/earshot-worker --version
USER earshot
VOLUME /data
HEALTHCHECK --interval=1m --timeout=10s --retries=2 --start-period=30s CMD ["/usr/local/bin/earshot-worker", "health"]
ENV EARSHOT_WORKER_DATA=/data
ENTRYPOINT ["/usr/local/bin/earshot-worker"]
CMD ["run"]
