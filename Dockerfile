# read5-uploader on its own, for running beside an rtl_airband that is not
# docker-rtlsdrairband (for that, see docker/rtlsdrairband/). rtl_airband's
# udp_stream datagrams have to reach this container: the simplest way is
# `network_mode: host` on both.
#
# The build context is the repository root (scripts/docker.sh, or Compose's
# `build:` with the repository's URL).

# The uploader binary. scripts/docker.sh cross-compiles it first into
# dist/docker/<arch>/ and it is only copied; without that (a build straight
# from the repository) it is compiled here, for the machine doing the build.
FROM debian:bookworm AS uploader
ARG TARGETARCH
WORKDIR /src
COPY . .
# gcc's spelling; .cargo/config.toml has zig's, for the cross-compile.
ENV CFLAGS_armv7_unknown_linux_gnueabihf="-mcpu=cortex-a7 -mfpu=neon-vfpv4"
RUN if [ -f "dist/docker/${TARGETARCH}/read5-uploader" ]; then \
      cp "dist/docker/${TARGETARCH}/read5-uploader" /read5-uploader; \
    else \
      apt-get update && \
      apt-get install -y --no-install-recommends ca-certificates curl build-essential cmake pkg-config && \
      curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal && \
      /root/.cargo/bin/cargo build --release --locked && \
      cp target/release/read5-uploader /read5-uploader; \
    fi && \
    chmod 755 /read5-uploader

FROM debian:bookworm-slim

COPY --from=uploader /read5-uploader /usr/local/bin/read5-uploader
COPY LICENSE THIRD_PARTY_LICENSES.md /usr/share/doc/read5-uploader/
RUN mkdir -p /etc/read5 /var/lib/read5 && \
    /usr/local/bin/read5-uploader --version

# The config is /etc/read5/read5-uploader.toml (mount it); the key is the
# READ5_UPLOAD_KEY environment variable. Spool and status file go here:
VOLUME /var/lib/read5

HEALTHCHECK --start-period=30s --interval=60s --timeout=5s CMD ["read5-uploader", "health"]

ENTRYPOINT ["read5-uploader"]
CMD ["run"]

LABEL org.opencontainers.image.source="https://github.com/jimvella/read5-uploader" \
      org.opencontainers.image.description="Records rtl_airband's UDP audio streams and uploads them to Read5" \
      org.opencontainers.image.licenses="MIT"
