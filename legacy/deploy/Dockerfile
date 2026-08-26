# Stage 1: Build
FROM golang:1.24-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    libpcap-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download

COPY . .
ARG VERSION=dev
RUN CGO_ENABLED=1 GOOS=linux GOARCH=amd64 \
    go build -ldflags="-s -w -X main.Version=${VERSION}" \
    -o /bin/cuma ./cmd/cuma

# Stage 2: Runtime
# Cannot use scratch - libpcap.so.0.8 must be present at runtime
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    libpcap0.8 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /data /etc/cuma

COPY --from=builder /bin/cuma /usr/local/bin/cuma
COPY configs/config.yaml /etc/cuma/config.yaml
COPY data/oui.csv /etc/cuma/oui.csv

EXPOSE 8080
VOLUME ["/data"]

# ARP scanning requires CAP_NET_RAW + CAP_NET_ADMIN (set in docker run / k8s securityContext)
ENTRYPOINT ["/usr/local/bin/cuma"]
CMD ["--config", "/etc/cuma/config.yaml"]
