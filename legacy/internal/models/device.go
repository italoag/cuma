package models

import (
	"database/sql/driver"
	"encoding/json"
	"fmt"
	"time"

	"github.com/google/uuid"
	"gorm.io/gorm"
)

type DeviceStatus string

const (
	StatusOnline  DeviceStatus = "online"
	StatusOffline DeviceStatus = "offline"
)

type DiscoveryMethod string

const (
	DiscoveryARP      DiscoveryMethod = "arp"
	DiscoveryMDNS     DiscoveryMethod = "mdns"
	DiscoverySSSDP    DiscoveryMethod = "ssdp"
	DiscoveryPortScan DiscoveryMethod = "port_scan"
)

// StringSlice persists []string as a JSON text column in SQLite.
type StringSlice []string

func (s StringSlice) Value() (driver.Value, error) {
	if s == nil {
		return "[]", nil
	}
	b, err := json.Marshal(s)
	return string(b), err
}

func (s *StringSlice) Scan(src interface{}) error {
	var str string
	switch v := src.(type) {
	case string:
		str = v
	case []byte:
		str = string(v)
	default:
		return fmt.Errorf("unsupported type: %T", src)
	}
	return json.Unmarshal([]byte(str), s)
}

type Device struct {
	ID            string       `gorm:"primaryKey" json:"id"`
	IP            string       `gorm:"index;not null" json:"ip"`
	MAC           string       `gorm:"index" json:"mac"`
	Hostname      string       `json:"hostname"`
	Manufacturer  string       `json:"manufacturer"`
	DeviceType    string       `json:"device_type"`
	Services      []Service    `gorm:"foreignKey:DeviceID" json:"services"`
	DiscoveredVia StringSlice  `gorm:"type:text" json:"discovered_via"`
	Status        DeviceStatus `json:"status"`
	FirstSeen     time.Time    `json:"first_seen"`
	LastSeen      time.Time    `json:"last_seen"`
	UserLabel     string       `json:"user_label"`
	Tags          StringSlice  `gorm:"type:text" json:"tags"`
	Confidence    float32      `json:"confidence"`
	CreatedAt     time.Time    `json:"created_at"`
	UpdatedAt     time.Time    `json:"updated_at"`
	DeletedAt     gorm.DeletedAt `gorm:"index" json:"-"`
}

func (d *Device) BeforeCreate(_ *gorm.DB) error {
	if d.ID == "" {
		d.ID = uuid.New().String()
	}
	return nil
}
