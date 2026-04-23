package api

import (
	"github.com/gin-gonic/gin"
	"github.com/italoag/cuma/internal/api/handlers"
	"github.com/italoag/cuma/internal/api/middleware"
	"github.com/italoag/cuma/internal/config"
	"github.com/italoag/cuma/internal/hub"
	"github.com/italoag/cuma/internal/scanner"
	"github.com/italoag/cuma/internal/store"
)

func NewRouter(
	cfg *config.Config,
	s store.Store,
	o *scanner.Orchestrator,
	h *hub.Hub,
	version string,
) *gin.Engine {
	gin.SetMode(gin.ReleaseMode)
	r := gin.New()
	r.Use(gin.Recovery())
	r.Use(middleware.CORS())
	r.Use(middleware.RateLimit(20))

	healthHandler := handlers.NewHealthHandler(version)
	devicesHandler := handlers.NewDevicesHandler(s)
	scanHandler := handlers.NewScanHandler(o)
	authHandler := handlers.NewAuthHandler(cfg.Auth)
	wsHandler := handlers.NewWebSocketHandler(h)

	// Public endpoints
	r.GET("/api/v1/health", healthHandler.Get)
	r.POST("/api/v1/auth/token", authHandler.Token)

	// Protected endpoints
	protected := r.Group("/api/v1")
	protected.Use(middleware.Auth(cfg.Auth))

	protected.POST("/auth/refresh", authHandler.Refresh)

	protected.GET("/devices", devicesHandler.List)
	protected.GET("/devices/:id", devicesHandler.Get)
	protected.PUT("/devices/:id", devicesHandler.Update)

	protected.POST("/scan", scanHandler.Start)
	protected.GET("/scan/status", scanHandler.Status)

	protected.GET("/events", wsHandler.Events)

	return r
}
