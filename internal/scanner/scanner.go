package scanner

import (
	"context"
	"fmt"
	"net"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
	"github.com/italoag/cuma/internal/config"
	"github.com/italoag/cuma/internal/hub"
	"github.com/italoag/cuma/internal/models"
	"github.com/italoag/cuma/internal/oui"
	"github.com/italoag/cuma/internal/store"
)

// ErrScanInProgress is returned when a scan is already running.
var ErrScanInProgress = fmt.Errorf("scan already in progress")

// Orchestrator coordinates all scanning phases.
type Orchestrator struct {
	cfg           config.ScannerConfig
	store         store.Store
	hub           *hub.Hub
	activeJob     atomic.Pointer[models.ScanJob]
	arpScanner    *ARPScanner
	mdnsScanner   *MDNSScanner
	ssdpScanner   *SSDPScanner
	portScanner   *PortScanner
	bannerGrabber *BannerGrabber
}

func NewOrchestrator(cfg config.ScannerConfig, s store.Store, h *hub.Hub) *Orchestrator {
	return &Orchestrator{
		cfg:           cfg,
		store:         s,
		hub:           h,
		arpScanner:    NewARPScanner(cfg.ARPTimeout),
		mdnsScanner:   NewMDNSScanner(cfg.MDNSTimeout),
		ssdpScanner:   NewSSDPScanner(cfg.SSDPTimeout),
		portScanner:   NewPortScanner(cfg.PortScanWorkers, cfg.PortScanTimeout),
		bannerGrabber: NewBannerGrabber(cfg.BannerTimeout),
	}
}

func (o *Orchestrator) StartScan(ctx context.Context, req models.ScanRequest) (*models.ScanJob, error) {
	job := &models.ScanJob{
		ID:        uuid.New().String(),
		Status:    models.ScanStatusPending,
		StartedAt: time.Now().UTC(),
	}

	if !o.activeJob.CompareAndSwap(nil, job) {
		return o.activeJob.Load(), ErrScanInProgress
	}

	iface := req.Interface
	if iface == "" {
		iface = o.cfg.DefaultInterface
	}
	if iface == "" {
		var err error
		iface, err = detectInterface()
		if err != nil {
			o.activeJob.Store(nil)
			return nil, err
		}
	}

	cidr := req.SubnetCIDR
	if cidr == "" {
		var err error
		cidr, err = detectCIDR(iface)
		if err != nil {
			o.activeJob.Store(nil)
			return nil, err
		}
	}

	job.Interface = iface
	job.SubnetCIDR = cidr
	job.Status = models.ScanStatusRunning

	o.hub.Broadcast(hub.EventScanStarted, job)

	go o.runScan(ctx, job, req)
	return job, nil
}

// ActiveJob returns the currently active or most recent scan job.
func (o *Orchestrator) ActiveJob() *models.ScanJob {
	return o.activeJob.Load()
}

func (o *Orchestrator) runScan(ctx context.Context, job *models.ScanJob, _ models.ScanRequest) {
	defer func() {
		now := time.Now().UTC()
		job.CompletedAt = &now
		if job.Status != models.ScanStatusFailed {
			job.Status = models.ScanStatusCompleted
		}
		o.hub.Broadcast(hub.EventScanCompleted, job)
		time.AfterFunc(30*time.Second, func() { o.activeJob.Store(nil) })
	}()

	deviceMap := make(map[string]*deviceAccumulator)
	var mu sync.Mutex

	merge := func(r models.ScanResult) {
		mu.Lock()
		defer mu.Unlock()
		acc, ok := deviceMap[r.IP]
		if !ok {
			acc = &deviceAccumulator{ip: r.IP}
			deviceMap[r.IP] = acc
		}
		acc.merge(r)
	}

	// Phase 1: ARP
	job.CurrentPhase = "arp"
	job.Progress = 5
	o.hub.Broadcast(hub.EventScanProgress, progressPayload(job))

	arpCh, _ := o.arpScanner.Scan(ctx, job.Interface, job.SubnetCIDR)
	for r := range arpCh {
		merge(r)
	}

	// Phase 2: mDNS + SSDP concurrent
	job.Progress = 25
	job.CurrentPhase = "mdns_ssdp"
	o.hub.Broadcast(hub.EventScanProgress, progressPayload(job))

	mdnsCh, _ := o.mdnsScanner.Scan(ctx)
	ssdpCh, _ := o.ssdpScanner.Scan(ctx)

	var wg2 sync.WaitGroup
	wg2.Add(2)
	go func() {
		defer wg2.Done()
		for r := range mdnsCh {
			merge(r)
		}
	}()
	go func() {
		defer wg2.Done()
		for r := range ssdpCh {
			merge(r)
		}
	}()
	wg2.Wait()

	// Phase 3: Port scan
	job.Progress = 50
	job.CurrentPhase = "port_scan"
	o.hub.Broadcast(hub.EventScanProgress, progressPayload(job))

	mu.Lock()
	var ips []string
	for ip := range deviceMap {
		ips = append(ips, ip)
	}
	mu.Unlock()

	portCh := o.portScanner.ScanIPs(ctx, ips, o.cfg.TargetPorts)
	for r := range portCh {
		merge(r)
	}

	// Phase 4: Banner grabbing
	job.Progress = 75
	job.CurrentPhase = "banner"
	o.hub.Broadcast(hub.EventScanProgress, progressPayload(job))

	sem := make(chan struct{}, 5)
	var wgB sync.WaitGroup

	mu.Lock()
	bannerTargets := make(map[string]int)
	for ip, acc := range deviceMap {
		for _, svc := range acc.services {
			if svc.Port == 80 || svc.Port == 8080 || svc.Port == 8443 || svc.Port == 443 {
				bannerTargets[ip] = svc.Port
				break
			}
		}
	}
	mu.Unlock()

	for ip, port := range bannerTargets {
		sem <- struct{}{}
		wgB.Add(1)
		go func(ip string, port int) {
			defer wgB.Done()
			defer func() { <-sem }()
			br, err := o.bannerGrabber.Grab(ip, port)
			if err != nil {
				return
			}
			mu.Lock()
			if a, ok := deviceMap[ip]; ok {
				a.httpBanner = br.CombinedText()
			}
			mu.Unlock()
		}(ip, port)
	}
	wgB.Wait()

	// Finalize
	job.Progress = 90
	job.CurrentPhase = "finalize"
	o.hub.Broadcast(hub.EventScanProgress, progressPayload(job))

	saveCtx := context.Background()
	mu.Lock()
	defer mu.Unlock()

	for _, acc := range deviceMap {
		device := acc.toDevice()
		if err := o.store.UpsertDevice(saveCtx, device); err != nil {
			continue
		}
		job.DevicesFound++
		o.hub.Broadcast(hub.EventDeviceDiscovered, device)
	}

	job.Progress = 100
}

type deviceAccumulator struct {
	ip           string
	mac          string
	hostname     string
	mdnsServices []string
	upnpDevice   string
	httpBanner   string
	services     []models.Service
	methods      []string
}

func (a *deviceAccumulator) merge(r models.ScanResult) {
	if r.MAC != "" && a.mac == "" {
		a.mac = r.MAC
	}
	if r.Hostname != "" && a.hostname == "" {
		a.hostname = r.Hostname
	}
	method := string(r.DiscoveredVia)
	if !containsStr(a.methods, method) {
		a.methods = append(a.methods, method)
	}
	for _, svc := range r.Services {
		a.services = append(a.services, svc)
	}
	if r.RawMetadata != nil {
		if svcs, ok := r.RawMetadata["mdns_services"].([]string); ok {
			a.mdnsServices = append(a.mdnsServices, svcs...)
		}
		if dt, ok := r.RawMetadata["upnp_device_type"].(string); ok && dt != "" {
			a.upnpDevice = dt
		}
	}
}

func (a *deviceAccumulator) toDevice() *models.Device {
	fp := Fingerprint(FingerprintInput{
		MAC:          a.mac,
		MDNSServices: a.mdnsServices,
		HTTPBanner:   a.httpBanner,
		OpenPorts:    portList(a.services),
		UPnPDevice:   a.upnpDevice,
	})

	manufacturer := fp.Manufacturer
	if manufacturer == "" {
		manufacturer = oui.Lookup(a.mac)
	}

	now := time.Now().UTC()
	return &models.Device{
		IP:            a.ip,
		MAC:           a.mac,
		Hostname:      a.hostname,
		Manufacturer:  manufacturer,
		DeviceType:    fp.DeviceType,
		Services:      a.services,
		DiscoveredVia: models.StringSlice(a.methods),
		Status:        models.StatusOnline,
		FirstSeen:     now,
		LastSeen:      now,
		Confidence:    fp.Confidence,
	}
}

func portList(services []models.Service) []int {
	ports := make([]int, 0, len(services))
	for _, s := range services {
		ports = append(ports, s.Port)
	}
	return ports
}

func containsStr(ss []string, s string) bool {
	for _, v := range ss {
		if v == s {
			return true
		}
	}
	return false
}

func progressPayload(job *models.ScanJob) map[string]interface{} {
	return map[string]interface{}{
		"scan_id":       job.ID,
		"progress":      job.Progress,
		"phase":         job.CurrentPhase,
		"devices_found": job.DevicesFound,
	}
}

func detectInterface() (string, error) {
	ifaces, err := net.Interfaces()
	if err != nil {
		return "", err
	}
	for _, iface := range ifaces {
		if iface.Flags&net.FlagLoopback != 0 || iface.Flags&net.FlagUp == 0 {
			continue
		}
		addrs, _ := iface.Addrs()
		for _, addr := range addrs {
			if ip, _, err := net.ParseCIDR(addr.String()); err == nil && ip.To4() != nil && !ip.IsLoopback() {
				return iface.Name, nil
			}
		}
	}
	return "", fmt.Errorf("no suitable network interface found")
}

func detectCIDR(ifaceName string) (string, error) {
	iface, err := net.InterfaceByName(ifaceName)
	if err != nil {
		return "", err
	}
	addrs, err := iface.Addrs()
	if err != nil {
		return "", err
	}
	for _, addr := range addrs {
		if strings.Contains(addr.String(), ".") {
			_, network, err := net.ParseCIDR(addr.String())
			if err == nil {
				return network.String(), nil
			}
		}
	}
	return "", fmt.Errorf("no IPv4 address on interface %s", ifaceName)
}
