FROM alpine:3.20 AS build
RUN apk add --no-cache gcc musl-dev
COPY libvmcall.h shutdown.c /src/
RUN gcc -O2 -static -o /bedrock-shutdown /src/shutdown.c

FROM alpine:3.20
RUN apk add --no-cache curl jq
COPY --from=build /bedrock-shutdown /usr/local/bin/bedrock-shutdown
COPY check.sh /usr/local/bin/check-tempo
ENTRYPOINT ["/bin/sh", "/usr/local/bin/check-tempo"]
