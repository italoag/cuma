package middleware

import (
	"net/http"
	"strings"

	"github.com/gin-gonic/gin"
	"github.com/golang-jwt/jwt/v5"
	"github.com/italoag/cuma/internal/config"
)

const claimsKey = "claims"

// Auth returns a middleware that validates API Key or JWT based on auth mode.
func Auth(cfg config.AuthConfig) gin.HandlerFunc {
	return func(c *gin.Context) {
		if cfg.Mode == "disabled" {
			c.Next()
			return
		}

		// Try API Key first
		if cfg.Mode == "apikey" || cfg.Mode == "both" {
			if key := c.GetHeader("X-API-Key"); key != "" {
				for _, valid := range cfg.APIKeys {
					if key == valid {
						c.Next()
						return
					}
				}
				// Also check query param for WS connections
				if qkey := c.Query("api_key"); qkey != "" {
					for _, valid := range cfg.APIKeys {
						if qkey == valid {
							c.Next()
							return
						}
					}
				}
			}
		}

		// Try JWT
		if cfg.Mode == "jwt" || cfg.Mode == "both" {
			token := extractBearerToken(c)
			if token == "" {
				// Allow ?token= for WebSocket
				token = c.Query("token")
			}
			if token != "" {
				claims, err := validateJWT(token, cfg.JWTSecret)
				if err == nil {
					c.Set(claimsKey, claims)
					c.Next()
					return
				}
			}
		}

		c.AbortWithStatusJSON(http.StatusUnauthorized, gin.H{"error": "unauthorized"})
	}
}

func extractBearerToken(c *gin.Context) string {
	h := c.GetHeader("Authorization")
	if strings.HasPrefix(h, "Bearer ") {
		return strings.TrimPrefix(h, "Bearer ")
	}
	return ""
}

func validateJWT(tokenStr, secret string) (jwt.MapClaims, error) {
	token, err := jwt.Parse(tokenStr, func(t *jwt.Token) (interface{}, error) {
		if _, ok := t.Method.(*jwt.SigningMethodHMAC); !ok {
			return nil, jwt.ErrSignatureInvalid
		}
		return []byte(secret), nil
	})
	if err != nil || !token.Valid {
		return nil, err
	}
	claims, ok := token.Claims.(jwt.MapClaims)
	if !ok {
		return nil, jwt.ErrTokenInvalidClaims
	}
	return claims, nil
}
