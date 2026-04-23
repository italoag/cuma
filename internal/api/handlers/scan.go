package handlers

import (
	"net/http"

	"github.com/gin-gonic/gin"
	"github.com/italoag/cuma/internal/api/dto"
	"github.com/italoag/cuma/internal/models"
	"github.com/italoag/cuma/internal/scanner"
)

type ScanHandler struct {
	orchestrator *scanner.Orchestrator
}

func NewScanHandler(o *scanner.Orchestrator) *ScanHandler {
	return &ScanHandler{orchestrator: o}
}

func (h *ScanHandler) Start(c *gin.Context) {
	var reqDTO dto.ScanRequestDTO
	// allow empty body
	_ = c.ShouldBindJSON(&reqDTO)

	req := models.ScanRequest{
		Interface:  reqDTO.Interface,
		SubnetCIDR: reqDTO.SubnetCIDR,
		Methods:    reqDTO.Methods,
		Timeout:    reqDTO.Timeout,
	}

	job, err := h.orchestrator.StartScan(c.Request.Context(), req)
	if err == scanner.ErrScanInProgress {
		c.JSON(http.StatusConflict, gin.H{
			"error":   "scan already in progress",
			"scan_id": job.ID,
		})
		return
	}
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}

	c.JSON(http.StatusAccepted, job)
}

func (h *ScanHandler) Status(c *gin.Context) {
	job := h.orchestrator.ActiveJob()
	if job == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "no scan has been run yet"})
		return
	}
	c.JSON(http.StatusOK, job)
}
