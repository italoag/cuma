package store

import (
	"context"
	"math"
	"strings"
	"sync"
	"time"

	"github.com/italoag/cuma/internal/models"
	"gorm.io/gorm"
)

// MemoryStore is used in unit tests; no persistence.
type MemoryStore struct {
	mu      sync.RWMutex
	devices map[string]*models.Device
}

func NewMemoryStore() *MemoryStore {
	return &MemoryStore{devices: make(map[string]*models.Device)}
}

func (m *MemoryStore) ListDevices(_ context.Context, filter ListDevicesFilter) (*ListDevicesResult, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()

	page := filter.Page
	if page < 1 {
		page = 1
	}
	perPage := filter.PerPage
	if perPage < 1 || perPage > 200 {
		perPage = 50
	}

	var all []models.Device
	for _, d := range m.devices {
		if filter.Status != "" && filter.Status != "all" && string(d.Status) != filter.Status {
			continue
		}
		if filter.Type != "" && d.DeviceType != filter.Type {
			continue
		}
		if filter.Search != "" {
			q := strings.ToLower(filter.Search)
			if !strings.Contains(strings.ToLower(d.IP), q) &&
				!strings.Contains(strings.ToLower(d.Hostname), q) &&
				!strings.Contains(strings.ToLower(d.UserLabel), q) &&
				!strings.Contains(strings.ToLower(d.Manufacturer), q) {
				continue
			}
		}
		all = append(all, *d)
	}

	total := int64(len(all))
	totalPages := int(math.Ceil(float64(total) / float64(perPage)))
	start := (page - 1) * perPage
	end := start + perPage
	if start > len(all) {
		start = len(all)
	}
	if end > len(all) {
		end = len(all)
	}

	return &ListDevicesResult{
		Devices:    all[start:end],
		Total:      total,
		Page:       page,
		PerPage:    perPage,
		TotalPages: totalPages,
	}, nil
}

func (m *MemoryStore) GetDevice(_ context.Context, id string) (*models.Device, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	d, ok := m.devices[id]
	if !ok {
		return nil, gorm.ErrRecordNotFound
	}
	cp := *d
	return &cp, nil
}

func (m *MemoryStore) GetDeviceByIP(_ context.Context, ip string) (*models.Device, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	for _, d := range m.devices {
		if d.IP == ip {
			cp := *d
			return &cp, nil
		}
	}
	return nil, gorm.ErrRecordNotFound
}

func (m *MemoryStore) GetDeviceByMAC(_ context.Context, mac string) (*models.Device, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	for _, d := range m.devices {
		if d.MAC == mac {
			cp := *d
			return &cp, nil
		}
	}
	return nil, gorm.ErrRecordNotFound
}

func (m *MemoryStore) UpsertDevice(_ context.Context, device *models.Device) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	cp := *device
	m.devices[device.ID] = &cp
	return nil
}

func (m *MemoryStore) UpdateDevice(_ context.Context, id string, label string, tags []string) (*models.Device, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	d, ok := m.devices[id]
	if !ok {
		return nil, gorm.ErrRecordNotFound
	}
	d.UserLabel = label
	d.Tags = tags
	cp := *d
	return &cp, nil
}

func (m *MemoryStore) DeleteDevice(_ context.Context, id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.devices, id)
	return nil
}

func (m *MemoryStore) MarkOffline(_ context.Context, before interface{}) error {
	cutoff, ok := before.(time.Time)
	if !ok {
		return nil
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	for _, d := range m.devices {
		if d.LastSeen.Before(cutoff) && d.Status == models.StatusOnline {
			d.Status = models.StatusOffline
		}
	}
	return nil
}

func (m *MemoryStore) Close() error { return nil }
