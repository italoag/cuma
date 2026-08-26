package scanner

import (
	"bufio"
	"context"
	"encoding/binary"
	"fmt"
	"net"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/google/gopacket"
	"github.com/google/gopacket/layers"
	"github.com/google/gopacket/pcap"
	"github.com/italoag/cuma/internal/models"
)

type ARPEntry struct {
	IP  string
	MAC string
}

type ARPScanner struct {
	timeout time.Duration
}

func NewARPScanner(timeout time.Duration) *ARPScanner {
	return &ARPScanner{timeout: timeout}
}

// Scan sweeps the given subnet CIDR on the given interface.
// Falls back to /proc/net/arp when gopacket/pcap fails.
func (s *ARPScanner) Scan(ctx context.Context, iface string, cidr string) (<-chan models.ScanResult, error) {
	results := make(chan models.ScanResult, 64)

	go func() {
		defer close(results)

		// Try pcap-based ARP sweep first
		if err := s.scanPCAP(ctx, iface, cidr, results); err != nil {
			// Fallback to kernel ARP table (no raw socket required)
			entries, procErr := readKernelARP()
			if procErr != nil {
				return
			}
			for _, e := range entries {
				select {
				case <-ctx.Done():
					return
				case results <- toScanResult(e.IP, e.MAC):
				}
			}
		}
	}()

	return results, nil
}

func (s *ARPScanner) scanPCAP(ctx context.Context, iface, cidr string, results chan<- models.ScanResult) error {
	handle, err := pcap.OpenLive(iface, 65536, true, pcap.BlockForever)
	if err != nil {
		return fmt.Errorf("pcap open: %w", err)
	}
	defer handle.Close()

	if err := handle.SetBPFFilter("arp"); err != nil {
		return fmt.Errorf("bpf filter: %w", err)
	}

	_, ipNet, err := net.ParseCIDR(cidr)
	if err != nil {
		return fmt.Errorf("parse cidr: %w", err)
	}

	netIface, err := net.InterfaceByName(iface)
	if err != nil {
		return fmt.Errorf("interface: %w", err)
	}

	srcMAC := netIface.HardwareAddr
	var srcIP net.IP
	addrs, _ := netIface.Addrs()
	for _, a := range addrs {
		if ip, _, err := net.ParseCIDR(a.String()); err == nil && ip.To4() != nil {
			srcIP = ip.To4()
			break
		}
	}
	if srcIP == nil {
		return fmt.Errorf("no IPv4 address on %s", iface)
	}

	seen := make(map[string]bool)
	var mu sync.Mutex

	// Reader goroutine
	readerDone := make(chan struct{})
	go func() {
		defer close(readerDone)
		src := gopacket.NewPacketSource(handle, handle.LinkType())
		for {
			select {
			case <-ctx.Done():
				return
			case pkt, ok := <-src.Packets():
				if !ok {
					return
				}
				arpLayer := pkt.Layer(layers.LayerTypeARP)
				if arpLayer == nil {
					continue
				}
				arp := arpLayer.(*layers.ARP)
				if arp.Operation != layers.ARPReply {
					continue
				}
				ip := net.IP(arp.SourceProtAddress).String()
				mac := net.HardwareAddr(arp.SourceHwAddress).String()
				mu.Lock()
				if !seen[ip] {
					seen[ip] = true
					mu.Unlock()
					select {
					case results <- toScanResult(ip, mac):
					case <-ctx.Done():
						return
					}
				} else {
					mu.Unlock()
				}
			}
		}
	}()

	// Send ARP requests to all hosts in subnet
	hosts := enumerate(ipNet)
	for _, host := range hosts {
		select {
		case <-ctx.Done():
			goto done
		default:
		}
		if err := sendARP(handle, srcMAC, srcIP, host); err != nil {
			continue
		}
	}

	// Wait for replies
	select {
	case <-ctx.Done():
	case <-time.After(s.timeout):
	}

done:
	handle.Close()
	<-readerDone
	return nil
}

func sendARP(handle *pcap.Handle, srcMAC net.HardwareAddr, srcIP, dstIP net.IP) error {
	eth := layers.Ethernet{
		SrcMAC:       srcMAC,
		DstMAC:       net.HardwareAddr{0xff, 0xff, 0xff, 0xff, 0xff, 0xff},
		EthernetType: layers.EthernetTypeARP,
	}
	arp := layers.ARP{
		AddrType:          layers.LinkTypeEthernet,
		Protocol:          layers.EthernetTypeIPv4,
		HwAddressSize:     6,
		ProtAddressSize:   4,
		Operation:         layers.ARPRequest,
		SourceHwAddress:   []byte(srcMAC),
		SourceProtAddress: []byte(srcIP.To4()),
		DstHwAddress:      []byte{0, 0, 0, 0, 0, 0},
		DstProtAddress:    []byte(dstIP.To4()),
	}
	buf := gopacket.NewSerializeBuffer()
	opts := gopacket.SerializeOptions{FixLengths: true, ComputeChecksums: true}
	if err := gopacket.SerializeLayers(buf, opts, &eth, &arp); err != nil {
		return err
	}
	return handle.WritePacketData(buf.Bytes())
}

// enumerate returns all host IPs in the network (excluding network and broadcast).
func enumerate(n *net.IPNet) []net.IP {
	ip := n.IP.Mask(n.Mask)
	// convert to uint32 for iteration
	start := binary.BigEndian.Uint32(ip.To4())
	mask := binary.BigEndian.Uint32([]byte(n.Mask))
	end := (start & mask) | ^mask

	var ips []net.IP
	for i := start + 1; i < end; i++ {
		b := make([]byte, 4)
		binary.BigEndian.PutUint32(b, i)
		ips = append(ips, net.IP(b))
	}
	return ips
}

// readKernelARP reads /proc/net/arp for ARP cache entries.
func readKernelARP() ([]ARPEntry, error) {
	f, err := os.Open("/proc/net/arp")
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var entries []ARPEntry
	scanner := bufio.NewScanner(f)
	scanner.Scan() // header
	for scanner.Scan() {
		fields := strings.Fields(scanner.Text())
		if len(fields) < 4 {
			continue
		}
		ip := fields[0]
		mac := fields[3]
		flags := fields[2]
		// 0x0 = incomplete, skip
		if mac == "00:00:00:00:00:00" || flags == "0x0" {
			continue
		}
		entries = append(entries, ARPEntry{IP: ip, MAC: mac})
	}
	return entries, nil
}

func toScanResult(ip, mac string) models.ScanResult {
	return models.ScanResult{
		IP:            ip,
		MAC:           mac,
		DiscoveredVia: models.DiscoveryARP,
	}
}
