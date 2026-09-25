package dto

type ScanRequestDTO struct {
	Interface  string   `json:"interface,omitempty"`
	SubnetCIDR string   `json:"subnet_cidr,omitempty"`
	Methods    []string `json:"methods,omitempty"`
	Timeout    int      `json:"timeout_seconds,omitempty"`
}
