package models

import "gorm.io/gorm"

type Service struct {
	gorm.Model
	DeviceID string `gorm:"index" json:"device_id"`
	Type     string `json:"type"`     // http, mqtt, coap, upnp, etc.
	Port     int    `json:"port"`
	Protocol string `json:"protocol"` // tcp, udp
	Banner   string `json:"banner"`
	Metadata string `json:"metadata,omitempty"` // JSON blob
}
