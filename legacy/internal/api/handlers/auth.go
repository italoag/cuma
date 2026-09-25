package handlers

import (
	"net/http"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/golang-jwt/jwt/v5"
	"github.com/italoag/cuma/internal/config"
)

type AuthHandler struct {
	cfg config.AuthConfig
}

func NewAuthHandler(cfg config.AuthConfig) *AuthHandler {
	return &AuthHandler{cfg: cfg}
}

type loginRequest struct {
	Username string `json:"username" binding:"required"`
	Password string `json:"password" binding:"required"`
}

func (h *AuthHandler) Token(c *gin.Context) {
	var req loginRequest
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	if req.Username != h.cfg.AdminUsername || req.Password != h.cfg.AdminPassword {
		c.JSON(http.StatusUnauthorized, gin.H{"error": "invalid credentials"})
		return
	}

	token, expiresAt, err := h.generateToken(req.Username)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "failed to generate token"})
		return
	}

	c.JSON(http.StatusOK, gin.H{
		"token":      token,
		"expires_at": expiresAt,
	})
}

func (h *AuthHandler) Refresh(c *gin.Context) {
	// Extract username from JWT claims set by auth middleware
	claimsVal, exists := c.Get("claims")
	if !exists {
		c.JSON(http.StatusUnauthorized, gin.H{"error": "unauthorized"})
		return
	}
	claims, ok := claimsVal.(jwt.MapClaims)
	if !ok {
		c.JSON(http.StatusUnauthorized, gin.H{"error": "invalid token claims"})
		return
	}

	username, _ := claims["sub"].(string)
	token, expiresAt, err := h.generateToken(username)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "failed to generate token"})
		return
	}

	c.JSON(http.StatusOK, gin.H{
		"token":      token,
		"expires_at": expiresAt,
	})
}

func (h *AuthHandler) generateToken(username string) (string, time.Time, error) {
	expiresAt := time.Now().Add(h.cfg.TokenTTL)
	claims := jwt.MapClaims{
		"sub": username,
		"exp": expiresAt.Unix(),
		"iat": time.Now().Unix(),
	}
	token := jwt.NewWithClaims(jwt.SigningMethodHS256, claims)
	signed, err := token.SignedString([]byte(h.cfg.JWTSecret))
	return signed, expiresAt, err
}
