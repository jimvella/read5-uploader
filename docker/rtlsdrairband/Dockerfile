# docker-rtlsdrairband with read5-uploader added as one more service, so one
# container receives air-band audio and contributes it to Read5. Nothing of
# the base image is changed except that rtl_airband is rebuilt for 16 kHz
# audio (see NFM below).
#
# The build context is the repository root: scripts/docker.sh, or in Compose
#   build:
#     context: https://github.com/jimvella/read5-uploader.git#main
#     dockerfile: docker/rtlsdrairband/Dockerfile
ARG BASE=ghcr.io/sdr-enthusiasts/docker-rtlsdrairband:latest

# The uploader binary. scripts/docker.sh cross-compiles it first into
# dist/docker/<arch>/ and it is only copied; without that (a build straight
# from the repository, as Compose's `build:` does) it is compiled here, for
# the machine doing the build. That takes a Pi 4 a while, once.
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

FROM ${BASE}

# rtl_airband's audio rate is 8 kHz, or 16 kHz when built with NFM support
# (AM channels included). The base image only does that build at container
# start, on every new container, when NFM_MAKE is set. Do it here once
# instead. Build with --build-arg NFM= to keep the base image's 8 kHz binary.
ARG NFM=1
RUN if [ -n "$NFM" ]; then \
      rm -f /usr/local/bin/rtl_airband && \
      NFM_MAKE=1 /scripts/build-rtl_airband.sh && \
      /usr/local/bin/rtl_airband -v; \
    fi

COPY --from=uploader /read5-uploader /usr/local/bin/read5-uploader
COPY LICENSE THIRD_PARTY_LICENSES.md /usr/share/doc/read5-uploader/
COPY docker/rtlsdrairband/rootfs/ /
RUN chmod 755 /etc/s6-overlay/scripts/read5-uploader \
      /etc/s6-overlay/s6-rc.d/read5-uploader/run \
      /scripts/healthcheck-read5.sh && \
    mkdir -p /var/lib/read5 && \
    /usr/local/bin/read5-uploader --version

# Audio waiting to be sealed or uploaded, and the status file. Mount a volume
# here so a backlog survives the container being replaced.
VOLUME /var/lib/read5

HEALTHCHECK --start-period=300s --interval=300s CMD ["/scripts/healthcheck-read5.sh"]

LABEL org.opencontainers.image.source="https://github.com/jimvella/read5-uploader" \
      org.opencontainers.image.description="docker-rtlsdrairband with the Read5 uploader" \
      org.opencontainers.image.licenses="GPL-3.0-only AND MIT"
