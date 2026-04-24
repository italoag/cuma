package scanner

import (
	"context"
	"fmt"
	"net"
	"sync"
	"time"

	"github.com/italoag/cuma/internal/models"
)

type PortScanner struct {
	workers int
	timeout time.Duration
}

func NewPortScanner(workers int, timeout time.Duration) *PortScanner {
	if workers <= 0 {
		workers = 20
	}
	return &PortScanner{workers: workers, timeout: timeout}
}

type portTarget struct {
	ip   string
	port int
}

// ScanIPs scans a list of IPs for the given ports and emits results for each open port found.
func (s *PortScanner) ScanIPs(ctx context.Context, ips []string, ports []int) <-chan models.ScanResult {
	results := make(chan models.ScanResult, 128)

	go func() {
		defer close(results)

		work := make(chan portTarget, len(ips)*len(ports))
		for _, ip := range ips {
			for _, port := range ports {
				work <- portTarget{ip, port}
			}
		}
		close(work)

		var wg sync.WaitGroup
		// Aggregate open ports per IP before emitting
		openPorts := make(map[string][]int)
		var mu sync.Mutex

		for i := 0; i < s.workers; i++ {
			wg.Add(1)
			go func() {
				defer wg.Done()
				for {
					select {
					case <-ctx.Done():
						return
					case target, ok := <-work:
						if !ok {
							return
						}
						if isOpen(target.ip, target.port, s.timeout) {
							mu.Lock()
							openPorts[target.ip] = append(openPorts[target.ip], target.port)
							mu.Unlock()
						}
					}
				}
			}()
		}
		wg.Wait()

		for ip, ports := range openPorts {
			var services []models.Service
			for _, p := range ports {
				services = append(services, models.Service{
					// DeviceID is intentionally empty here; UpsertDevice assigns it
					Type:     portToServiceType(p),
					Port:     p,
					Protocol: "tcp",
				})
			}
			select {
			case <-ctx.Done():
				return
			case results <- models.ScanResult{
				IP:            ip,
				DiscoveredVia: models.DiscoveryPortScan,
				Services:      services,
			}:
			}
		}
	}()

	return results
}

func isOpen(ip string, port int, timeout time.Duration) bool {
	addr := fmt.Sprintf("%s:%d", ip, port)
	conn, err := net.DialTimeout("tcp", addr, timeout)
	if err != nil {
		return false
	}
	conn.Close()
	return true
}

func portToServiceType(port int) string {
	switch port {
	case 80:
		return "http"
	case 443:
		return "https"
	case 1883:
		return "mqtt"
	case 8883:
		return "mqtt-tls"
	case 5683:
		return "coap"
	case 5684:
		return "coaps"
	case 5000:
		return "http-alt"
	case 8080:
		return "http-alt"
	case 8443:
		return "https-alt"
	case 9000:
		return "http-alt"
	default:
		return "unknown"
	}
}
