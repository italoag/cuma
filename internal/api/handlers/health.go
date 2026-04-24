package handlers

import (
	"net/http"
	"time"

	"github.com/gin-gonic/gin"
)

var startTime = time.Now()

// HealthHandler returns service health status.
type HealthHandler struct {
	version string
}

func NewHealthHandler(version string) *HealthHandler {
	return &HealthHandler{version: version}
}

func (h *HealthHandler) Get(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{
		"status":          "ok",
		"version":         h.version,
		"uptime_seconds":  int(time.Since(startTime).Seconds()),
	})
}
