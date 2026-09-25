package store

import (
	"context"
	"math"
	"strings"
	"time"

	"github.com/italoag/cuma/internal/config"
	"github.com/italoag/cuma/internal/models"
	"gorm.io/driver/sqlite"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

type SQLiteStore struct {
	db *gorm.DB
}

func NewSQLiteStore(cfg config.DatabaseConfig) (*SQLiteStore, error) {
	db, err := gorm.Open(sqlite.Open(cfg.DSN), &gorm.Config{
		Logger: logger.Default.LogMode(logger.Silent),
	})
	if err != nil {
		return nil, err
	}

	sqlDB, err := db.DB()
	if err != nil {
		return nil, err
	}

	sqlDB.SetMaxOpenConns(1)
	db.Exec("PRAGMA journal_mode=WAL")
	db.Exec("PRAGMA synchronous=NORMAL")
	db.Exec("PRAGMA busy_timeout=5000")

	if err := db.AutoMigrate(&models.Device{}, &models.Service{}); err != nil {
		return nil, err
	}

	return &SQLiteStore{db: db}, nil
}

func (s *SQLiteStore) ListDevices(_ context.Context, filter ListDevicesFilter) (*ListDevicesResult, error) {
	page := filter.Page
	if page < 1 {
		page = 1
	}
	perPage := filter.PerPage
	if perPage < 1 || perPage > 200 {
		perPage = 50
	}

	q := s.db.Model(&models.Device{})

	if filter.Status != "" && filter.Status != "all" {
		q = q.Where("status = ?", filter.Status)
	}
	if filter.Type != "" {
		q = q.Where("device_type = ?", filter.Type)
	}
	if filter.Search != "" {
		like := "%" + strings.ToLower(filter.Search) + "%"
		q = q.Where("lower(ip) LIKE ? OR lower(hostname) LIKE ? OR lower(user_label) LIKE ? OR lower(manufacturer) LIKE ?",
			like, like, like, like)
	}

	var total int64
	if err := q.Count(&total).Error; err != nil {
		return nil, err
	}

	var devices []models.Device
	offset := (page - 1) * perPage
	if err := q.Preload("Services").Offset(offset).Limit(perPage).Order("last_seen DESC").Find(&devices).Error; err != nil {
		return nil, err
	}

	totalPages := int(math.Ceil(float64(total) / float64(perPage)))

	return &ListDevicesResult{
		Devices:    devices,
		Total:      total,
		Page:       page,
		PerPage:    perPage,
		TotalPages: totalPages,
	}, nil
}

func (s *SQLiteStore) GetDevice(_ context.Context, id string) (*models.Device, error) {
	var d models.Device
	if err := s.db.Preload("Services").First(&d, "id = ?", id).Error; err != nil {
		return nil, err
	}
	return &d, nil
}

func (s *SQLiteStore) GetDeviceByIP(_ context.Context, ip string) (*models.Device, error) {
	var d models.Device
	if err := s.db.Preload("Services").First(&d, "ip = ?", ip).Error; err != nil {
		return nil, err
	}
	return &d, nil
}

func (s *SQLiteStore) GetDeviceByMAC(_ context.Context, mac string) (*models.Device, error) {
	var d models.Device
	if err := s.db.Preload("Services").First(&d, "mac = ?", mac).Error; err != nil {
		return nil, err
	}
	return &d, nil
}

func (s *SQLiteStore) UpsertDevice(_ context.Context, device *models.Device) error {
	var existing models.Device
	key := device.MAC
	col := "mac"
	if key == "" {
		key = device.IP
		col = "ip"
	}

	err := s.db.Where(col+" = ?", key).First(&existing).Error
	if err == gorm.ErrRecordNotFound {
		return s.db.Create(device).Error
	}
	if err != nil {
		return err
	}

	device.ID = existing.ID
	device.FirstSeen = existing.FirstSeen // preserve original discovery time
	device.UserLabel = existing.UserLabel
	device.Tags = existing.Tags
	// device.LastSeen comes from the caller (scanner sets it to time.Now())

	if err := s.db.Where("device_id = ?", existing.ID).Delete(&models.Service{}).Error; err != nil {
		return err
	}
	for i := range device.Services {
		device.Services[i].DeviceID = existing.ID
	}

	return s.db.Save(device).Error
}

func (s *SQLiteStore) UpdateDevice(_ context.Context, id string, label string, tags []string) (*models.Device, error) {
	result := s.db.Model(&models.Device{}).Where("id = ?", id).Updates(map[string]interface{}{
		"user_label": label,
		"tags":       models.StringSlice(tags),
	})
	if result.Error != nil {
		return nil, result.Error
	}
	if result.RowsAffected == 0 {
		return nil, gorm.ErrRecordNotFound
	}
	var d models.Device
	if err := s.db.Preload("Services").First(&d, "id = ?", id).Error; err != nil {
		return nil, err
	}
	return &d, nil
}

func (s *SQLiteStore) DeleteDevice(_ context.Context, id string) error {
	return s.db.Delete(&models.Device{}, "id = ?", id).Error
}

func (s *SQLiteStore) MarkOffline(_ context.Context, before interface{}) error {
	cutoff, ok := before.(time.Time)
	if !ok {
		return nil
	}
	return s.db.Model(&models.Device{}).
		Where("last_seen < ? AND status = ?", cutoff, models.StatusOnline).
		Update("status", models.StatusOffline).Error
}

func (s *SQLiteStore) Close() error {
	sqlDB, err := s.db.DB()
	if err != nil {
		return err
	}
	return sqlDB.Close()
}
