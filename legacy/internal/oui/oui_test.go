package oui_test

import (
	"testing"

	"github.com/italoag/cuma/internal/oui"
	"github.com/stretchr/testify/assert"
)

func TestLookup(t *testing.T) {
	tests := []struct {
		name     string
		mac      string
		wantHit  bool
		wantMfr  string
	}{
		{"raspberry pi", "B8:27:EB:aa:bb:cc", true, "Raspberry Pi Foundation"},
		{"espressif",    "28:5F:CB:11:22:33", true, "Espressif Inc."},
		{"unknown",      "FF:FF:FF:AA:BB:CC", false, ""},
		{"empty",        "", false, ""},
		{"colons",       "DC:A6:32:01:02:03", true, "Raspberry Pi Trading Ltd"},
		{"dashes",       "DC-A6-32-01-02-03", true, "Raspberry Pi Trading Ltd"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := oui.Lookup(tt.mac)
			if tt.wantHit {
				assert.Equal(t, tt.wantMfr, got)
			} else {
				assert.Empty(t, got)
			}
		})
	}
}
