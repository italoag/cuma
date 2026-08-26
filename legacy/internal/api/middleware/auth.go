package middleware

import (
	"log"
	"net/http"
	"strings"

	"github.com/gin-gonic/gin"
	"github.com/golang-jwt/jwt/v5"
	"github.com/italoag/cuma/internal/config"
)

const claimsKey = "claims"

// Auth returns a middleware that validates API Key or JWT based on auth mode.
// It logs a warning at startup if API key mode is enabled but no keys are configured.
func Auth(cfg config.AuthConfig) gin.HandlerFunc {
	if (cfg.Mode == "apikey" || cfg.Mode == "both") && len(cfg.APIKeys) == 0 {
		log.Println("[WARN] auth mode includes 'apikey' but CUMA_AUTH_API_KEYS is empty — API key auth will always fail")
	}

	return func(c *gin.Context) {
		if cfg.Mode == "disabled" {
			c.Next()
			return
		}

		// Try API Key first (header, then query param for WebSocket)
		if cfg.Mode == "apikey" || cfg.Mode == "both" {
			candidate := c.GetHeader("X-API-Key")
			if candidate == "" {
				candidate = c.Query("api_key")
			}
			if candidate != "" {
				for _, valid := range cfg.APIKeys {
					if candidate == valid {
						c.Next()
						return
					}
				}
			}
		}

		// Try JWT (header, then query param for WebSocket)
		if cfg.Mode == "jwt" || cfg.Mode == "both" {
			token := extractBearerToken(c)
			if token == "" {
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
