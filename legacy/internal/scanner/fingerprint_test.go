package scanner_test

import (
	"testing"

	"github.com/italoag/cuma/internal/scanner"
	"github.com/stretchr/testify/assert"
)

func TestFingerprint(t *testing.T) {
	tests := []struct {
		name       string
		input      scanner.FingerprintInput
		wantType   string
		minConfidence float32
	}{
		{
			name:          "homekit via mdns",
			input:         scanner.FingerprintInput{MDNSServices: []string{"mydevice._hap._tcp.local."}},
			wantType:      "homekit_accessory",
			minConfidence: 1.0,
		},
		{
			name:          "chromecast via mdns",
			input:         scanner.FingerprintInput{MDNSServices: []string{"Living Room._googlecast._tcp.local."}},
			wantType:      "chromecast",
			minConfidence: 1.0,
		},
		{
			name:          "philips hue via http banner",
			input:         scanner.FingerprintInput{HTTPBanner: "server: philips hue bridge"},
			wantType:      "smart_light",
			minConfidence: 0.9,
		},
		{
			name:          "raspberry pi via mac oui",
			input:         scanner.FingerprintInput{MAC: "B8:27:EB:01:02:03"},
			wantType:      "single_board_computer",
			minConfidence: 0.6,
		},
		{
			name:          "mqtt broker via port",
			input:         scanner.FingerprintInput{OpenPorts: []int{1883}},
			wantType:      "mqtt_broker",
			minConfidence: 0.5,
		},
		{
			name:          "unknown device",
			input:         scanner.FingerprintInput{},
			wantType:      "unknown",
			minConfidence: 0.0,
		},
		{
			name:          "openwrt router via banner",
			input:         scanner.FingerprintInput{HTTPBanner: "openwrt/23.05"},
			wantType:      "router",
			minConfidence: 0.8,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := scanner.Fingerprint(tt.input)
			assert.Equal(t, tt.wantType, got.DeviceType, "device type mismatch")
			assert.GreaterOrEqual(t, got.Confidence, tt.minConfidence, "confidence too low")
		})
	}
}
