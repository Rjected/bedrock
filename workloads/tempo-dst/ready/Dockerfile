# syntax=docker/dockerfile:1
#
# Signals the bedrock VM ready once the node is producing blocks, so the lab's
# initial checkpoint is taken on a live chain, then idles. Same ready helper as
# workloads/integration-tests; build.sh stages libvmcall.h and ready.c here.

FROM alpine:3.20 AS build
RUN apk add --no-cache gcc musl-dev
COPY libvmcall.h ready.c /src/
RUN gcc -O2 -static -o /bedrock-ready /src/ready.c

FROM alpine:3.20
COPY --from=build /bedrock-ready /usr/local/bin/bedrock-ready
COPY wait-and-ready.sh /usr/local/bin/wait-and-ready.sh
ENTRYPOINT ["/bin/sh", "/usr/local/bin/wait-and-ready.sh"]
