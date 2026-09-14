# syntax=docker/dockerfile:1
FROM scratch
ARG TARGETPLATFORM
ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="juicity-rs" \
      org.opencontainers.image.source="https://github.com/juicity/juicity-rs" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --chmod=755 dist/${TARGETPLATFORM}/juicity-server /usr/local/bin/juicity-server
USER 65532:65532
EXPOSE 23182/udp
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/juicity-server"]
CMD ["run", "-c", "/etc/juicity/server.json"]
