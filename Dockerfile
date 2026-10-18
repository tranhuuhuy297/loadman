# Image published with each release: ghcr.io/tranhuuhuy297/loadman
# It packages the prebuilt Linux release binaries, which the release workflow
# places in dist/amd64 and dist/arm64.
#
#   docker run --rm -v "$PWD:/work" ghcr.io/tranhuuhuy297/loadman run collection.json -u 20 -t 1m
#   docker run -p 7070:7070 -e LOADMAN_AGENT_TOKEN ghcr.io/tranhuuhuy297/loadman agent
FROM gcr.io/distroless/cc-debian12:nonroot
ARG TARGETARCH
COPY dist/${TARGETARCH}/loadman /usr/local/bin/loadman
WORKDIR /work
# `loadman agent` listens here by default.
EXPOSE 7070
ENTRYPOINT ["/usr/local/bin/loadman"]
