# read5-uploader on its own, for running beside an rtl_airband that is not
# docker-rtlsdrairband (for that, see docker/rtlsdrairband/). rtl_airband's
# udp_stream datagrams have to reach this container: the simplest way is
# `network_mode: host` on both.
#
# Built by scripts/docker.sh, which cross-compiles the binary first into
# dist/docker/<arch>/. The build context is the repository root.
FROM debian:bookworm-slim

ARG TARGETARCH
COPY dist/docker/${TARGETARCH}/read5-uploader /usr/local/bin/read5-uploader
COPY LICENSE THIRD_PARTY_LICENSES.md /usr/share/doc/read5-uploader/
RUN chmod 755 /usr/local/bin/read5-uploader && \
    mkdir -p /etc/read5 /var/lib/read5 && \
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
