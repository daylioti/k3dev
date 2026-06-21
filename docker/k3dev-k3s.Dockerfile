# k3dev custom k3s image.
#
# Layers k3dev's helper binaries onto the upstream rancher/k3s image so a fresh
# cluster comes up with them already present (no runtime `docker cp` injection):
#   - socat        : Docker API relay
#   - k3dev-agent  : in-container CPU/mem stats collector
#
# Built multi-arch (linux/amd64, linux/arm64) by .github/workflows/build-k3s-images.yml.
# The image tag mirrors the upstream rancher/k3s tag exactly (e.g. v1.35.2-k3s1),
# so only the registry/repo changes for consumers.
#
# Build context must contain the helper binaries named by Docker's TARGETARCH:
#   socat-amd64        socat-arm64
#   k3dev-agent-amd64  k3dev-agent-arm64

ARG K3S_VERSION
FROM rancher/k3s:${K3S_VERSION}

# TARGETARCH is provided automatically by buildx per --platform (amd64 | arm64).
ARG TARGETARCH

COPY --chmod=0755 socat-${TARGETARCH}       /usr/local/bin/socat
COPY --chmod=0755 k3dev-agent-${TARGETARCH} /usr/local/bin/k3dev-agent
