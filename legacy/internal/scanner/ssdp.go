package scanner

import (
	"context"
	"encoding/xml"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/italoag/cuma/internal/models"
	"github.com/koron/go-ssdp"
)

type SSDPScanner struct {
	timeout time.Duration
}

func NewSSDPScanner(timeout time.Duration) *SSDPScanner {
	return &SSDPScanner{timeout: timeout}
}

func (s *SSDPScanner) Scan(ctx context.Context) (<-chan models.ScanResult, error) {
	results := make(chan models.ScanResult, 64)

	go func() {
		defer close(results)

		list, err := ssdp.Search(ssdp.All, int(s.timeout.Seconds()), "")
		if err != nil {
			return
		}

		seen := make(map[string]bool)
		client := &http.Client{Timeout: 5 * time.Second}

		for _, srv := range list {
			select {
			case <-ctx.Done():
				return
			default:
			}

			ip := extractIPFromURL(srv.Location)
			if ip == "" {
				continue
			}
			if seen[ip] {
				continue
			}
			seen[ip] = true

			result := models.ScanResult{
				IP:            ip,
				DiscoveredVia: models.DiscoverySSSDP,
				RawMetadata:   make(map[string]interface{}),
			}

			result.RawMetadata["usn"] = srv.USN
			result.RawMetadata["st"] = srv.Type
			result.RawMetadata["server"] = srv.Server
			result.RawMetadata["location"] = srv.Location

			// Fetch device description XML
			if srv.Location != "" {
				if desc, err := fetchUPnPDescription(client, srv.Location); err == nil {
					result.Hostname = desc.Device.FriendlyName
					result.RawMetadata["upnp_manufacturer"] = desc.Device.Manufacturer
					result.RawMetadata["upnp_model"] = desc.Device.ModelName
					result.RawMetadata["upnp_device_type"] = desc.Device.DeviceType
					if result.Services == nil {
						result.Services = []models.Service{{
							Type:     "upnp",
							Port:     80,
							Protocol: "tcp",
						}}
					}
				}
			}

			select {
			case results <- result:
			case <-ctx.Done():
				return
			}
		}
	}()

	return results, nil
}

type upnpRoot struct {
	Device upnpDevice `xml:"device"`
}

type upnpDevice struct {
	DeviceType   string `xml:"deviceType"`
	FriendlyName string `xml:"friendlyName"`
	Manufacturer string `xml:"manufacturer"`
	ModelName    string `xml:"modelName"`
	UDN          string `xml:"UDN"`
}

func fetchUPnPDescription(client *http.Client, location string) (*upnpRoot, error) {
	resp, err := client.Get(location)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(io.LimitReader(resp.Body, 64*1024))
	if err != nil {
		return nil, err
	}

	var root upnpRoot
	if err := xml.Unmarshal(body, &root); err != nil {
		return nil, err
	}
	return &root, nil
}

// extractIPFromURL extracts the hostname/IP from a URL like http://192.168.1.1:8080/desc.xml
func extractIPFromURL(rawURL string) string {
	if rawURL == "" {
		return ""
	}
	// strip scheme
	s := rawURL
	if idx := strings.Index(s, "://"); idx != -1 {
		s = s[idx+3:]
	}
	// strip path
	if idx := strings.Index(s, "/"); idx != -1 {
		s = s[:idx]
	}
	// strip port
	if idx := strings.LastIndex(s, ":"); idx != -1 {
		host := s[:idx]
		if strings.HasPrefix(host, "[") {
			return strings.Trim(host, "[]")
		}
		return host
	}
	return s
}
