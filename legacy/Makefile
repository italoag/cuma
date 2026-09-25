BINARY   := cuma
MODULE   := github.com/italoag/cuma
VERSION  := $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")
LDFLAGS  := -ldflags "-s -w -X main.Version=$(VERSION)"
BINDIR   := bin

.PHONY: all build run test test-int lint docker clean oui-update fmt

all: build

build:
	CGO_ENABLED=1 go build $(LDFLAGS) -o $(BINDIR)/$(BINARY) ./cmd/cuma

run: build
	@if [ -f .env ]; then set -a && . ./.env && set +a; fi && ./$(BINDIR)/$(BINARY) --config configs/config.yaml

test:
	go test ./internal/... -v -race -coverprofile=coverage.out

test-int:
	sudo go test ./... -v -tags integration -race

cover: test
	go tool cover -html=coverage.out

lint:
	golangci-lint run ./...

fmt:
	gofmt -w .

docker:
	docker build -f deploy/Dockerfile -t cuma:$(VERSION) .

docker-run: docker
	docker run --rm \
		--cap-add=NET_RAW \
		--cap-add=NET_ADMIN \
		--network=host \
		-e CUMA_AUTH_API_KEYS=dev-key \
		-v $(PWD)/data:/data \
		cuma:$(VERSION)

clean:
	rm -rf $(BINDIR)/ coverage.out

oui-update:
	bash scripts/fetch-oui.sh

$(BINDIR):
	mkdir -p $(BINDIR)
