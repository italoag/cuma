# CUMA - Cloud Unified Modeling Architecture
## Go IoT Network Scanner Service

### O que este projeto faz
Escaneia a rede local para descobrir dispositivos IoT via ARP, mDNS, SSDP/UPnP,
port scanning e HTTP banner grabbing. Expõe REST API + WebSocket para um app iOS.

### Tech Stack
- Go 1.24, módulo: `github.com/italoag/cuma`
- HTTP framework: `github.com/gin-gonic/gin`
- WebSocket: `github.com/gorilla/websocket`
- ARP scanning: `github.com/google/gopacket` (requer libpcap-dev no build, libpcap0.8 no runtime)
- mDNS: `github.com/miekg/dns`
- SSDP/UPnP: `github.com/koron/go-ssdp`
- Auth JWT: `github.com/golang-jwt/jwt/v5`
- ORM/DB: `gorm.io/gorm` + `gorm.io/driver/sqlite`
- Config: `github.com/spf13/viper` + `github.com/joho/godotenv`

### Comandos principais
```bash
make build       # compilar binário em bin/cuma
make run         # build + rodar (carrega .env se existir)
make test        # testes unitários com race detector
make test-int    # testes de integração (precisa root para raw sockets)
make cover       # gera relatório HTML de cobertura
make lint        # golangci-lint
make oui-update  # atualiza data/oui.csv da IEEE (requer acesso à internet)
make docker      # build da imagem Docker
```

### Permissões críticas (ARP scanning)
ARP scanning usa raw sockets via gopacket/libpcap.
O processo DEVE rodar como root OU ter `CAP_NET_RAW` + `CAP_NET_ADMIN`.

```bash
# Opção 1: rodar como root
sudo ./bin/cuma --config configs/config.yaml

# Opção 2: capability no binário (persistente)
sudo setcap cap_net_raw,cap_net_admin+ep ./bin/cuma

# Opção 3: sem ARP (fallback automático para /proc/net/arp)
# Rodar como usuário normal - apenas lê cache ARP do kernel
```

Se pcap falhar (sem permissão), o scanner faz fallback automático para `/proc/net/arp`.

### Estrutura de packages
```
cmd/cuma/main.go              entry point, DI, signal handling, graceful shutdown
internal/config/config.go     Viper config (arquivo + env vars com prefixo CUMA_)
internal/models/              structs GORM: Device, Service, ScanJob, ScanRequest
internal/store/               interface Store + implementações: SQLite, Memory (testes)
internal/oui/oui.go           embedded OUI DB (data/oui.csv), lookup por MAC
internal/hub/hub.go           WebSocket broadcast hub (select loop goroutine única)
internal/scanner/
  arp.go                      ARP sweep via gopacket + fallback /proc/net/arp
  mdns.go                     mDNS queries para _http, _mqtt, _hap, _googlecast, etc.
  ssdp.go                     SSDP/UPnP discovery + parse XML description
  portscan.go                 TCP connect worker pool (N=20 por padrão)
  banner.go                   HTTP GET / com timeout curto; extrai Server header
  fingerprint.go              Classificador rule-based: device type + confidence score
  scanner.go                  Orchestrator: pipeline ARP→mDNS+SSDP→PortScan→Banner→Merge
internal/api/
  router.go                   Gin router + wiring de middleware e handlers
  middleware/auth.go           API Key (X-API-Key) e/ou JWT (Authorization: Bearer)
  middleware/cors.go           CORS headers para app iOS
  middleware/ratelimit.go      Token bucket por IP
  handlers/devices.go          GET /devices (paginado), GET /devices/:id, PUT /devices/:id
  handlers/scan.go             POST /scan (async), GET /scan/status
  handlers/auth.go             POST /auth/token (login), POST /auth/refresh
  handlers/health.go           GET /health (público)
  handlers/websocket.go        WS /events (stream em tempo real)
  dto/                         Structs de request/response
```

### Configuração (configs/config.yaml)
```yaml
server:
  port: 8080

scanner:
  default_interface: "eth0"   # deixar vazio para auto-detectar
  auto_scan_interval: "5m"    # "0s" = desabilitado

auth:
  mode: "both"                # apikey | jwt | both | disabled
  api_keys: []                # via CUMA_AUTH_API_KEYS (comma-separated)
  jwt_secret: "..."           # via CUMA_AUTH_JWT_SECRET

database:
  dsn: "./cuma.db"            # via CUMA_DATABASE_DSN
```

### Variáveis de ambiente (prefixo `CUMA_`)
| Variável | Descrição |
|----------|-----------|
| `CUMA_SERVER_PORT` | Porta HTTP (default: 8080) |
| `CUMA_AUTH_API_KEYS` | Chaves separadas por vírgula |
| `CUMA_AUTH_JWT_SECRET` | Secret do JWT |
| `CUMA_AUTH_ADMIN_PASSWORD` | Senha para POST /auth/token |
| `CUMA_DATABASE_DSN` | Path do SQLite (default: ./cuma.db) |
| `CUMA_SCANNER_DEFAULT_INTERFACE` | Interface de rede |
| `CUMA_LOG_LEVEL` | debug \| info \| warn \| error |

### API - Endpoints principais
| Método | Path | Auth | Descrição |
|--------|------|------|-----------|
| GET | `/api/v1/health` | Não | Status do serviço |
| POST | `/api/v1/auth/token` | Não | Login → JWT |
| POST | `/api/v1/auth/refresh` | Sim | Renovar JWT |
| GET | `/api/v1/devices` | Sim | Listar dispositivos (paginado) |
| GET | `/api/v1/devices/:id` | Sim | Detalhes do dispositivo |
| PUT | `/api/v1/devices/:id` | Sim | Atualizar label e tags |
| POST | `/api/v1/scan` | Sim | Disparar scan (async) |
| GET | `/api/v1/scan/status` | Sim | Progresso do scan |
| WS | `/api/v1/events` | Sim | Stream de eventos |

**Parâmetros de /devices:** `page`, `per_page`, `status` (online/offline/all), `type`, `q` (busca)

### Autenticação no app iOS
```swift
// API Key
request.setValue("minha-chave", forHTTPHeaderField: "X-API-Key")

// JWT (após POST /auth/token)
request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

// WebSocket com token
let url = URL(string: "ws://host:8080/api/v1/events?token=\(jwt)")!
```

### Arquitetura de scan (pipeline assíncrono)
```
POST /scan → Orchestrator.StartScan() → goroutine:
  1. ARP sweep (gopacket + /proc/net/arp fallback)
  2. mDNS queries + SSDP discovery (concorrentes)
  3. Port scan worker pool (N=20, TCP connect)
  4. HTTP banner grab (N=5 concorrente)
  5. Fingerprint + merge por IP/MAC
  6. Persist no SQLite, broadcast WS events
```

### Modelo de Device
```json
{
  "id": "uuid",
  "ip": "192.168.1.42",
  "mac": "dc:a6:32:ab:cd:ef",
  "hostname": "raspberrypi.local",
  "manufacturer": "Raspberry Pi Trading Ltd",
  "device_type": "single_board_computer",
  "status": "online",
  "confidence": 0.7,
  "discovered_via": ["arp", "mdns"],
  "services": [{"type": "http", "port": 80, "protocol": "tcp"}],
  "first_seen": "...",
  "last_seen": "...",
  "user_label": "Meu Pi",
  "tags": ["home", "dev"]
}
```

### Deploy Docker
```bash
docker build -f deploy/Dockerfile -t cuma:latest .
docker run --rm \
  --cap-add=NET_RAW --cap-add=NET_ADMIN \
  --network=host \
  -e CUMA_AUTH_API_KEYS=minha-chave \
  -v ./data:/data \
  cuma:latest
```

### Erros comuns a evitar
- **NÃO** usar `scratch` como base Docker — libpcap.so.0.8 deve estar presente
- **NÃO** desabilitar CGO — gopacket requer
- **NÃO** esquecer `PRAGMA journal_mode=WAL` no SQLite (já feito no store/sqlite.go)
- **mDNS** usa multicast UDP 224.0.0.251:5353 — firewall deve permitir
- **SSDP** usa multicast UDP 239.255.255.250:1900 — mesmo caveat
- **ARP** no Docker requer `--network=host` — bridge network bloqueia raw packets
- Um único ScanJob ativo por vez (enforced via `atomic.Pointer` + CAS em scanner.go)

### Testando
```bash
# Health check
curl http://localhost:8080/api/v1/health

# Login e obter JWT
curl -X POST http://localhost:8080/api/v1/auth/token \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"change-me"}'

# Listar dispositivos com API key
curl -H "X-API-Key: dev-key" http://localhost:8080/api/v1/devices

# Iniciar scan
curl -X POST -H "X-API-Key: dev-key" http://localhost:8080/api/v1/scan

# Status do scan
curl -H "X-API-Key: dev-key" http://localhost:8080/api/v1/scan/status
```
