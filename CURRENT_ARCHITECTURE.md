# Current architecture (pre-transformation)

A record of what this repository was before the transformation, written from an
inspection of the code at commit `1b736b5`. The code itself is preserved under
[`legacy/`](legacy/) rather than deleted, so this document can be checked
against the thing it describes.

## What it was

**CUMA — Cloud Unified Modeling Architecture**: a Go service that scanned a
local network for IoT devices and exposed the results over REST and WebSocket
for an iOS app.

| Property | Value |
|---|---|
| Language | Go 1.24 |
| Module | `github.com/italoag/cuma` |
| Size | ~3,100 lines across 29 files |
| Tests | 3 test files (`store`, `oui`, `fingerprint`) |
| CI | None |
| Deployment | Dockerfile, docker-compose, Kubernetes manifests |

## Structure

```
cmd/cuma/main.go              entry point, dependency injection, graceful shutdown
internal/config/              Viper configuration (file + CUMA_ env vars)
internal/models/              GORM structs: Device, Service, ScanJob, ScanRequest
internal/store/               Store interface + SQLite and in-memory implementations
internal/oui/                 embedded IEEE OUI database, MAC vendor lookup
internal/hub/                 WebSocket broadcast hub (single goroutine, select loop)
internal/scanner/             ARP, mDNS, SSDP, port scan, banner grab, fingerprint
internal/api/                 Gin router, middleware (auth, CORS, rate limit), handlers
```

## How it worked

```
POST /scan
   └─> Orchestrator.StartScan()  (one job at a time, atomic.Pointer + CAS)
         1. ARP sweep            gopacket/libpcap, /proc/net/arp fallback
         2. mDNS + SSDP          concurrent multicast discovery
         3. Port scan            TCP connect worker pool (N=20)
         4. Banner grab          HTTP GET, Server header (N=5)
         5. Fingerprint          rule-based classification + confidence score
         6. Persist + broadcast  SQLite write, WebSocket event
```

## Assessment

The code was competent for what it was. Several decisions are worth naming
because they informed the new design rather than being discarded with it:

- **A pipeline orchestrator with a single active job**, enforced by
  compare-and-swap rather than a mutex.
- **An interface-backed store** with a real and an in-memory implementation, so
  tests never touched SQLite.
- **A broadcast hub owning its own state in one goroutine**, so publishers never
  block on a slow subscriber.
- **Layered configuration** from a file plus prefixed environment variables.
- **Graceful shutdown** driven by signal handling and context cancellation.

What it was not: nothing in the domain — devices, services, scan jobs, MAC
vendor lookup — has any meaning in an agent orchestrator, and nothing in the
transport layer (multicast discovery, raw sockets, libpcap) transfers either.

## Component classification

Per the migration brief, every component was classified before anything moved.

| Component | Verdict | Reasoning |
|---|---|---|
| `internal/scanner/*` | **REMOVE** | ARP, mDNS, SSDP, port scanning and banner grabbing have no analogue in agent orchestration. |
| `internal/oui/*` | **REMOVE** | A MAC-vendor database is specific to network scanning. |
| `internal/models/*` | **REMOVE** | Device, Service and ScanJob do not map onto Task, Agent or Model. |
| `internal/api/*` | **REMOVE** | The new product's interfaces are a TUI, a CLI and ACP/A2A servers — not a REST API for an iOS client. |
| `internal/store/*` | **REPLACE** | The *pattern* (interface + real + in-memory) carries over into `cuma-core`'s ports and `cuma-persistence`. The Go implementation does not. |
| `internal/hub/*` | **REPLACE** | Superseded by `cuma-core::EventBus`, which serves the same purpose (fan-out that cannot be stalled by a slow reader) with a richer event vocabulary. |
| `internal/config/*` | **REPLACE** | The layered file-plus-environment idea carries over into `cuma-config`; Viper does not. |
| `internal/scanner/scanner.go` | **REPLACE** | The single-active-job orchestrator pattern informed `cuma-orchestrator`, which needs a task DAG rather than a fixed pipeline. |
| `deploy/*` | **REFACTOR** | Container and Kubernetes manifests are reusable in shape, but the new binary has entirely different runtime requirements: no libpcap, no `CAP_NET_RAW`, no host networking. |
| `Makefile` | **REFACTOR** | The target vocabulary (`build`, `test`, `lint`, `cover`) is worth keeping; the Go toolchain invocations are not. |
| `CLAUDE.md` | **REPLACE** | Rewritten for the new product. |
| Everything else | **REMOVE** | |

Nothing was classified **KEEP**. That is the honest conclusion: the two products
share a repository, a name and an author, and nothing else. What transferred
were patterns and judgement, not code.

## Risks identified before starting

| Risk | Mitigation taken |
|---|---|
| Destroying working code that someone still depends on | The Go tree was moved to `legacy/`, not deleted. It still builds from that directory. |
| Rewriting into a language the toolchain cannot build | Rust 1.94.1 verified present before any code was written. |
| Depending on SDKs that do not exist or whose APIs were assumed | Every dependency was resolved against the crates.io index and its vendored source read before use. See [`DEPENDENCY_ANALYSIS.md`](DEPENDENCY_ANALYSIS.md). |
| Producing documentation instead of a working system | Each milestone landed as compiling, tested code before the next began. |
| A "big bang" rewrite that never reaches a runnable state | Vertical slices: the foundation ran end-to-end against mock agents before any protocol adapter existed. |

## What survived

- The repository, its name and its history.
- The `legacy/` tree, buildable and unchanged.
- Five structural patterns, listed above, re-expressed in Rust.
