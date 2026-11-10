# docker-rtlsdrairband with read5-uploader added as one more service, so one
# container receives air-band audio and contributes it to Read5. Nothing of
# the base image is changed except that rtl_airband is rebuilt for 16 kHz
# audio (see NFM below).
#
# Built by scripts/docker.sh, which cross-compiles the uploader first into
# dist/docker/<arch>/. The build context is the repository root.
ARG BASE=ghcr.io/sdr-enthusiasts/docker-rtlsdrairband:latest
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

ARG TARGETARCH
COPY dist/docker/${TARGETARCH}/read5-uploader /usr/local/bin/read5-uploader
COPY LICENSE THIRD_PARTY_LICENSES.md /usr/share/doc/read5-uploader/
COPY docker/rtlsdrairband/rootfs/ /
RUN chmod 755 /usr/local/bin/read5-uploader \
      /etc/s6-overlay/scripts/read5-uploader \
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
