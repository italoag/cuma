package models

import "time"

type ScanStatus string

const (
	ScanStatusPending   ScanStatus = "pending"
	ScanStatusRunning   ScanStatus = "running"
	ScanStatusCompleted ScanStatus = "completed"
	ScanStatusFailed    ScanStatus = "failed"
)

type ScanJob struct {
	ID           string     `json:"id"`
	Status       ScanStatus `json:"status"`
	Progress     int        `json:"progress"`
	CurrentPhase string     `json:"current_phase"`
	DevicesFound int        `json:"devices_found"`
	StartedAt    time.Time  `json:"started_at"`
	CompletedAt  *time.Time `json:"completed_at,omitempty"`
	Error        string     `json:"error,omitempty"`
	SubnetCIDR   string     `json:"subnet_cidr"`
	Interface    string     `json:"interface"`
}

type ScanRequest struct {
	Interface  string   `json:"interface,omitempty"`
	SubnetCIDR string   `json:"subnet_cidr,omitempty"`
	Methods    []string `json:"methods,omitempty"`
	Timeout    int      `json:"timeout_seconds,omitempty"`
}

type ScanResult struct {
	IP            string
	MAC           string
	Hostname      string
	DiscoveredVia DiscoveryMethod
	Services      []Service
	RawMetadata   map[string]interface{}
}
