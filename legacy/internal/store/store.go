package store

import (
	"context"

	"github.com/italoag/cuma/internal/models"
)

type ListDevicesFilter struct {
	Status  string
	Type    string
	Search  string
	Page    int
	PerPage int
}

type ListDevicesResult struct {
	Devices    []models.Device
	Total      int64
	Page       int
	PerPage    int
	TotalPages int
}

type Store interface {
	ListDevices(ctx context.Context, filter ListDevicesFilter) (*ListDevicesResult, error)
	GetDevice(ctx context.Context, id string) (*models.Device, error)
	GetDeviceByIP(ctx context.Context, ip string) (*models.Device, error)
	GetDeviceByMAC(ctx context.Context, mac string) (*models.Device, error)
	UpsertDevice(ctx context.Context, device *models.Device) error
	UpdateDevice(ctx context.Context, id string, label string, tags []string) (*models.Device, error)
	DeleteDevice(ctx context.Context, id string) error
	MarkOffline(ctx context.Context, before interface{}) error
	Close() error
}
