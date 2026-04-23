package handlers

import (
	"net/http"
	"strconv"

	"github.com/gin-gonic/gin"
	"github.com/italoag/cuma/internal/api/dto"
	"github.com/italoag/cuma/internal/store"
	"gorm.io/gorm"
)

type DevicesHandler struct {
	store store.Store
}

func NewDevicesHandler(s store.Store) *DevicesHandler {
	return &DevicesHandler{store: s}
}

func (h *DevicesHandler) List(c *gin.Context) {
	page, _ := strconv.Atoi(c.DefaultQuery("page", "1"))
	perPage, _ := strconv.Atoi(c.DefaultQuery("per_page", "50"))

	filter := store.ListDevicesFilter{
		Status:  c.Query("status"),
		Type:    c.Query("type"),
		Search:  c.Query("q"),
		Page:    page,
		PerPage: perPage,
	}

	result, err := h.store.ListDevices(c.Request.Context(), filter)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}

	c.JSON(http.StatusOK, gin.H{
		"data": result.Devices,
		"meta": dto.PaginationMeta{
			Total:      result.Total,
			Page:       result.Page,
			PerPage:    result.PerPage,
			TotalPages: result.TotalPages,
		},
	})
}

func (h *DevicesHandler) Get(c *gin.Context) {
	id := c.Param("id")
	device, err := h.store.GetDevice(c.Request.Context(), id)
	if err == gorm.ErrRecordNotFound {
		c.JSON(http.StatusNotFound, gin.H{"error": "device not found"})
		return
	}
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, device)
}

func (h *DevicesHandler) Update(c *gin.Context) {
	id := c.Param("id")
	var req dto.UpdateDeviceRequest
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	tags := req.Tags
	if tags == nil {
		tags = []string{}
	}

	device, err := h.store.UpdateDevice(c.Request.Context(), id, req.UserLabel, tags)
	if err == gorm.ErrRecordNotFound {
		c.JSON(http.StatusNotFound, gin.H{"error": "device not found"})
		return
	}
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, device)
}
