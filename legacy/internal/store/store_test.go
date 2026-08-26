package store_test

import (
	"context"
	"testing"
	"time"

	"github.com/italoag/cuma/internal/models"
	"github.com/italoag/cuma/internal/store"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func newMemStore() store.Store {
	return store.NewMemoryStore()
}

func makeDevice(ip, mac string) *models.Device {
	now := time.Now().UTC()
	return &models.Device{
		ID:           "test-" + ip,
		IP:           ip,
		MAC:          mac,
		Manufacturer: "Test Corp",
		DeviceType:   "router",
		Status:       models.StatusOnline,
		FirstSeen:    now,
		LastSeen:     now,
		Confidence:   0.8,
	}
}

func TestMemoryStore_CRUD(t *testing.T) {
	ctx := context.Background()
	s := newMemStore()

	d := makeDevice("192.168.1.1", "AA:BB:CC:DD:EE:FF")
	require.NoError(t, s.UpsertDevice(ctx, d))

	got, err := s.GetDevice(ctx, d.ID)
	require.NoError(t, err)
	assert.Equal(t, d.IP, got.IP)
	assert.Equal(t, d.MAC, got.MAC)

	got2, err := s.GetDeviceByIP(ctx, d.IP)
	require.NoError(t, err)
	assert.Equal(t, d.ID, got2.ID)

	got3, err := s.GetDeviceByMAC(ctx, d.MAC)
	require.NoError(t, err)
	assert.Equal(t, d.ID, got3.ID)
}

func TestMemoryStore_ListDevices(t *testing.T) {
	ctx := context.Background()
	s := newMemStore()

	for i := range 5 {
		d := makeDevice("192.168.1."+string(rune('1'+i)), "AA:BB:CC:DD:EE:0"+string(rune('0'+i)))
		require.NoError(t, s.UpsertDevice(ctx, d))
	}

	result, err := s.ListDevices(ctx, store.ListDevicesFilter{Page: 1, PerPage: 10})
	require.NoError(t, err)
	assert.Equal(t, int64(5), result.Total)
	assert.Len(t, result.Devices, 5)
}

func TestMemoryStore_UpdateDevice(t *testing.T) {
	ctx := context.Background()
	s := newMemStore()

	d := makeDevice("192.168.1.100", "11:22:33:44:55:66")
	require.NoError(t, s.UpsertDevice(ctx, d))

	updated, err := s.UpdateDevice(ctx, d.ID, "My Router", []string{"home", "network"})
	require.NoError(t, err)
	assert.Equal(t, "My Router", updated.UserLabel)
	assert.Equal(t, []string{"home", "network"}, []string(updated.Tags))
}

func TestMemoryStore_MarkOffline(t *testing.T) {
	ctx := context.Background()
	s := newMemStore()

	old := makeDevice("192.168.1.200", "AA:AA:AA:AA:AA:AA")
	old.LastSeen = time.Now().Add(-10 * time.Minute)
	require.NoError(t, s.UpsertDevice(ctx, old))

	recent := makeDevice("192.168.1.201", "BB:BB:BB:BB:BB:BB")
	recent.LastSeen = time.Now()
	require.NoError(t, s.UpsertDevice(ctx, recent))

	cutoff := time.Now().Add(-5 * time.Minute)
	require.NoError(t, s.MarkOffline(ctx, cutoff))

	gotOld, _ := s.GetDevice(ctx, old.ID)
	assert.Equal(t, models.StatusOffline, gotOld.Status)

	gotRecent, _ := s.GetDevice(ctx, recent.ID)
	assert.Equal(t, models.StatusOnline, gotRecent.Status)
}

func TestMemoryStore_NotFound(t *testing.T) {
	ctx := context.Background()
	s := newMemStore()

	_, err := s.GetDevice(ctx, "nonexistent")
	assert.Error(t, err)
}
