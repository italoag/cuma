package middleware

import (
	"net/http"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
)

type bucket struct {
	tokens   float64
	lastTime time.Time
	mu       sync.Mutex
}

type rateLimiter struct {
	buckets  map[string]*bucket
	mu       sync.RWMutex
	rate     float64 // tokens per second
	capacity float64
	idleTTL  time.Duration // remove buckets idle longer than this
}

func newRateLimiter(rps float64) *rateLimiter {
	rl := &rateLimiter{
		buckets:  make(map[string]*bucket),
		rate:     rps,
		capacity: rps * 5,
		idleTTL:  10 * time.Minute,
	}
	go rl.cleanup()
	return rl
}

// cleanup removes stale buckets (IPs not seen for idleTTL) every 5 minutes.
func (rl *rateLimiter) cleanup() {
	for range time.Tick(5 * time.Minute) {
		cutoff := time.Now().Add(-rl.idleTTL)
		rl.mu.Lock()
		for ip, b := range rl.buckets {
			b.mu.Lock()
			idle := b.lastTime.Before(cutoff)
			b.mu.Unlock()
			if idle {
				delete(rl.buckets, ip)
			}
		}
		rl.mu.Unlock()
	}
}

func (rl *rateLimiter) allow(ip string) bool {
	rl.mu.RLock()
	b, ok := rl.buckets[ip]
	rl.mu.RUnlock()

	if !ok {
		rl.mu.Lock()
		// double-check after upgrading to write lock
		b, ok = rl.buckets[ip]
		if !ok {
			b = &bucket{tokens: rl.capacity, lastTime: time.Now()}
			rl.buckets[ip] = b
		}
		rl.mu.Unlock()
	}

	b.mu.Lock()
	defer b.mu.Unlock()

	now := time.Now()
	elapsed := now.Sub(b.lastTime).Seconds()
	if elapsed > 0 {
		b.tokens = minF(rl.capacity, b.tokens+elapsed*rl.rate)
		b.lastTime = now
	}

	if b.tokens < 1 {
		return false
	}
	b.tokens--
	return true
}

func minF(a, b float64) float64 {
	if a < b {
		return a
	}
	return b
}

// RateLimit limits requests to rps per IP address.
func RateLimit(rps float64) gin.HandlerFunc {
	rl := newRateLimiter(rps)
	return func(c *gin.Context) {
		ip := c.ClientIP()
		if !rl.allow(ip) {
			c.AbortWithStatusJSON(http.StatusTooManyRequests, gin.H{"error": "rate limit exceeded"})
			return
		}
		c.Next()
	}
}
