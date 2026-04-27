FROM alpine:3.22

RUN apk add --no-cache ca-certificates && mkdir -p /config

ARG TARGETARCH
ARG BINARY_NAME

COPY --chmod=755 artifacts/${TARGETARCH}/${BINARY_NAME} /usr/local/bin/app
COPY --chmod=755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

USER 65532:65532

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
