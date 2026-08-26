package scanner

import (
	"context"
	"net"
	"strings"
	"time"

	"github.com/italoag/cuma/internal/models"
	"github.com/miekg/dns"
)

var mdnsServiceTypes = []string{
	"_http._tcp.local.",
	"_https._tcp.local.",
	"_mqtt._tcp.local.",
	"_hap._tcp.local.",
	"_googlecast._tcp.local.",
	"_airplay._tcp.local.",
	"_sonos._tcp.local.",
	"_hue._tcp.local.",
	"_axis-video._tcp.local.",
	"_printer._tcp.local.",
	"_ipp._tcp.local.",
	"_daap._tcp.local.",
	"_coap._udp.local.",
	"_device-info._tcp.local.",
}

type MDNSScanner struct {
	timeout time.Duration
}

func NewMDNSScanner(timeout time.Duration) *MDNSScanner {
	return &MDNSScanner{timeout: timeout}
}

func (s *MDNSScanner) Scan(ctx context.Context) (<-chan models.ScanResult, error) {
	results := make(chan models.ScanResult, 64)

	go func() {
		defer close(results)

		conn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4zero, Port: 0})
		if err != nil {
			return
		}
		defer conn.Close()

		mdnsAddr := &net.UDPAddr{
			IP:   net.ParseIP("224.0.0.251"),
			Port: 5353,
		}

		deadline := time.Now().Add(s.timeout)
		conn.SetDeadline(deadline)

		// Send PTR queries for all service types
		for _, svcType := range mdnsServiceTypes {
			select {
			case <-ctx.Done():
				return
			default:
			}
			msg := new(dns.Msg)
			msg.SetQuestion(svcType, dns.TypePTR)
			msg.RecursionDesired = false
			packed, err := msg.Pack()
			if err != nil {
				continue
			}
			conn.WriteToUDP(packed, mdnsAddr)
		}

		// Collect responses
		buf := make([]byte, 9000)
		seen := make(map[string]bool)

		for {
			select {
			case <-ctx.Done():
				return
			default:
			}

			n, remoteAddr, err := conn.ReadFromUDP(buf)
			if err != nil {
				return
			}

			var resp dns.Msg
			if err := resp.Unpack(buf[:n]); err != nil {
				continue
			}

			ip := remoteAddr.IP.String()
			if seen[ip] {
				continue
			}

			result := models.ScanResult{
				IP:            ip,
				DiscoveredVia: models.DiscoveryMDNS,
				RawMetadata:   make(map[string]interface{}),
			}

			var svcTypes []string
			for _, rr := range append(resp.Answer, resp.Extra...) {
				switch r := rr.(type) {
				case *dns.A:
					if ip == "" || ip == remoteAddr.IP.String() {
						result.IP = r.A.String()
					}
				case *dns.PTR:
					svcTypes = append(svcTypes, r.Hdr.Name)
				case *dns.SRV:
					result.Hostname = strings.TrimSuffix(r.Target, ".")
					result.Services = append(result.Services, models.Service{
						Type:     extractServiceType(r.Hdr.Name),
						Port:     int(r.Port),
						Protocol: extractProtocol(r.Hdr.Name),
					})
				case *dns.TXT:
					var txts []string
					for _, t := range r.Txt {
						txts = append(txts, t)
					}
					result.RawMetadata["txt"] = txts
				}
			}

			if len(svcTypes) > 0 {
				result.RawMetadata["mdns_services"] = svcTypes
				seen[ip] = true
				select {
				case results <- result:
				case <-ctx.Done():
					return
				}
			}
		}
	}()

	return results, nil
}

func extractServiceType(name string) string {
	parts := strings.SplitN(name, ".", 2)
	if len(parts) > 1 {
		return parts[1]
	}
	return name
}

func extractProtocol(name string) string {
	if strings.Contains(name, "._tcp") {
		return "tcp"
	}
	if strings.Contains(name, "._udp") {
		return "udp"
	}
	return "tcp"
}
