package oui

import (
	"bufio"
	"bytes"
	_ "embed"
	"strings"
	"sync"
)

//go:embed oui.csv
var ouiCSV []byte

var (
	once     sync.Once
	ouiTable map[string]string
)

func init() {
	once.Do(load)
}

func load() {
	ouiTable = make(map[string]string, 30000)
	scanner := bufio.NewScanner(bytes.NewReader(ouiCSV))
	scanner.Scan() // skip header line
	for scanner.Scan() {
		line := scanner.Text()
		parts := strings.SplitN(line, ",", 3)
		if len(parts) < 3 {
			continue
		}
		prefix := strings.ToUpper(strings.TrimSpace(parts[1]))
		manufacturer := strings.Trim(strings.TrimSpace(parts[2]), `"`)
		ouiTable[prefix] = manufacturer
	}
}

// Lookup returns the manufacturer name for a given MAC address.
// MAC can be in any common format (XX:XX:XX:XX:XX:XX, XX-XX-XX-XX-XX-XX, etc.).
// Returns empty string if not found.
func Lookup(mac string) string {
	once.Do(load)
	normalized := normalize(mac)
	if len(normalized) < 6 {
		return ""
	}
	prefix := normalized[:6]
	return ouiTable[prefix]
}

func normalize(mac string) string {
	var b strings.Builder
	for _, c := range strings.ToUpper(mac) {
		if (c >= '0' && c <= '9') || (c >= 'A' && c <= 'F') {
			b.WriteRune(c)
		}
	}
	return b.String()
}
