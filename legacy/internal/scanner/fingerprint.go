package scanner

import (
	"strings"

	"github.com/italoag/cuma/internal/models"
	"github.com/italoag/cuma/internal/oui"
)

type FingerprintInput struct {
	MAC           string
	Manufacturer  string
	MDNSServices  []string
	HTTPBanner    string
	OpenPorts     []int
	UPnPModel     string
	UPnPDevice    string
}

type FingerprintResult struct {
	DeviceType   string
	Manufacturer string
	Confidence   float32
}

func Fingerprint(input FingerprintInput) FingerprintResult {
	manufacturer := input.Manufacturer
	if manufacturer == "" && input.MAC != "" {
		manufacturer = oui.Lookup(input.MAC)
	}

	// mDNS service-based rules (highest confidence)
	for _, svc := range input.MDNSServices {
		svc = strings.ToLower(svc)
		switch {
		case strings.Contains(svc, "_hap._tcp"):
			return FingerprintResult{"homekit_accessory", manufacturer, 1.0}
		case strings.Contains(svc, "_googlecast._tcp"):
			return FingerprintResult{"chromecast", "Google, Inc.", 1.0}
		case strings.Contains(svc, "_airplay._tcp"):
			return FingerprintResult{"apple_tv", "Apple, Inc.", 1.0}
		case strings.Contains(svc, "_sonos._tcp"):
			return FingerprintResult{"smart_speaker", "Sonos, Inc.", 1.0}
		case strings.Contains(svc, "_hue._tcp"):
			return FingerprintResult{"smart_light", "Philips Lighting BV", 1.0}
		case strings.Contains(svc, "_printer._tcp"), strings.Contains(svc, "_ipp._tcp"):
			return FingerprintResult{"printer", manufacturer, 0.95}
		case strings.Contains(svc, "_mqtt._tcp"):
			return FingerprintResult{"mqtt_device", manufacturer, 0.9}
		case strings.Contains(svc, "_coap._udp"):
			return FingerprintResult{"coap_device", manufacturer, 0.9}
		case strings.Contains(svc, "_axis-video._tcp"):
			return FingerprintResult{"ip_camera", "Axis Communications", 1.0}
		case strings.Contains(svc, "_daap._tcp"):
			return FingerprintResult{"media_server", manufacturer, 0.85}
		}
	}

	// UPnP device type rules (from SSDP device description XML)
	if input.UPnPDevice != "" {
		dev := strings.ToLower(input.UPnPDevice)
		model := strings.ToLower(input.UPnPModel)
		switch {
		case strings.Contains(dev, "mediarenderer"):
			return FingerprintResult{"media_renderer", manufacturer, 0.95}
		case strings.Contains(dev, "mediaserver"):
			return FingerprintResult{"media_server", manufacturer, 0.95}
		case strings.Contains(dev, "basicdevice") && strings.Contains(model, "hue"):
			return FingerprintResult{"smart_light", "Philips Lighting BV", 0.95}
		case strings.Contains(dev, "basicdevice"):
			return FingerprintResult{"smart_device", manufacturer, 0.7}
		case strings.Contains(dev, "wfadevice"):
			return FingerprintResult{"wifi_device", manufacturer, 0.7}
		}
	}

	// HTTP banner rules
	banner := strings.ToLower(input.HTTPBanner)
	if banner != "" {
		switch {
		case strings.Contains(banner, "philips hue"):
			return FingerprintResult{"smart_light", "Philips Lighting BV", 0.95}
		case strings.Contains(banner, "sonos"):
			return FingerprintResult{"smart_speaker", "Sonos, Inc.", 0.95}
		case strings.Contains(banner, "openwrt"):
			return FingerprintResult{"router", manufacturer, 0.9}
		case strings.Contains(banner, "dd-wrt"):
			return FingerprintResult{"router", manufacturer, 0.9}
		case strings.Contains(banner, "hikvision"):
			return FingerprintResult{"ip_camera", "Hikvision Digital Technology", 0.95}
		case strings.Contains(banner, "dahua"):
			return FingerprintResult{"ip_camera", "Dahua Technology", 0.95}
		case strings.Contains(banner, "axis"):
			return FingerprintResult{"ip_camera", "Axis Communications", 0.9}
		case strings.Contains(banner, "nest"):
			return FingerprintResult{"thermostat", "Nest Labs Inc.", 0.85}
		case strings.Contains(banner, "shelly"):
			return FingerprintResult{"smart_switch", "Shelly", 0.9}
		case strings.Contains(banner, "tasmota"):
			return FingerprintResult{"smart_switch", manufacturer, 0.9}
		case strings.Contains(banner, "esphome"):
			return FingerprintResult{"iot_sensor", manufacturer, 0.9}
		case strings.Contains(banner, "mikrotik"):
			return FingerprintResult{"router", "MikroTik", 0.95}
		case strings.Contains(banner, "ubiquiti"):
			return FingerprintResult{"access_point", "Ubiquiti Networks Inc.", 0.9}
		}
	}

	// Port-based heuristics
	portSet := make(map[int]bool)
	for _, p := range input.OpenPorts {
		portSet[p] = true
	}
	if portSet[1883] || portSet[8883] {
		return FingerprintResult{"mqtt_broker", manufacturer, 0.6}
	}
	if portSet[5683] {
		return FingerprintResult{"coap_device", manufacturer, 0.6}
	}

	// Manufacturer-based fallback
	mfr := strings.ToLower(manufacturer)
	switch {
	case strings.Contains(mfr, "nest"):
		return FingerprintResult{"thermostat", manufacturer, 0.5}
	case strings.Contains(mfr, "ring"):
		return FingerprintResult{"doorbell", manufacturer, 0.5}
	case strings.Contains(mfr, "sonos"):
		return FingerprintResult{"smart_speaker", manufacturer, 0.5}
	case strings.Contains(mfr, "philips"):
		return FingerprintResult{"smart_device", manufacturer, 0.4}
	case strings.Contains(mfr, "raspberry"):
		return FingerprintResult{"single_board_computer", manufacturer, 0.7}
	case strings.Contains(mfr, "espressif"):
		return FingerprintResult{"iot_module", manufacturer, 0.6}
	case strings.Contains(mfr, "cisco") || strings.Contains(mfr, "netgear") ||
		strings.Contains(mfr, "ubiquiti") || strings.Contains(mfr, "tp-link") ||
		strings.Contains(mfr, "d-link"):
		return FingerprintResult{"network_device", manufacturer, 0.6}
	case strings.Contains(mfr, "apple"):
		return FingerprintResult{"apple_device", manufacturer, 0.4}
	case strings.Contains(mfr, "samsung"):
		return FingerprintResult{"samsung_device", manufacturer, 0.4}
	case strings.Contains(mfr, "amazon"):
		return FingerprintResult{"amazon_device", manufacturer, 0.4}
	case strings.Contains(mfr, "google"):
		return FingerprintResult{"google_device", manufacturer, 0.4}
	}

	return FingerprintResult{"unknown", manufacturer, 0.1}
}

// ContainsDiscoveryMethod checks if a discovery method is already in the slice.
func ContainsDiscoveryMethod(slice []string, method models.DiscoveryMethod) bool {
	for _, s := range slice {
		if s == string(method) {
			return true
		}
	}
	return false
}
