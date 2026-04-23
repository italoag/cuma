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
}

func newRateLimiter(rps float64) *rateLimiter {
	rl := &rateLimiter{
		buckets:  make(map[string]*bucket),
		rate:     rps,
		capacity: rps * 5,
	}
	// Cleanup goroutine
	go func() {
		for range time.Tick(5 * time.Minute) {
			rl.mu.Lock()
			rl.buckets = make(map[string]*bucket)
			rl.mu.Unlock()
		}
	}()
	return rl
}

func (rl *rateLimiter) allow(ip string) bool {
	rl.mu.RLock()
	b, ok := rl.buckets[ip]
	rl.mu.RUnlock()

	if !ok {
		rl.mu.Lock()
		b = &bucket{tokens: rl.capacity, lastTime: time.Now()}
		rl.buckets[ip] = b
		rl.mu.Unlock()
	}

	b.mu.Lock()
	defer b.mu.Unlock()

	now := time.Now()
	elapsed := now.Sub(b.lastTime).Seconds()
	b.tokens = min(rl.capacity, b.tokens+elapsed*rl.rate)
	b.lastTime = now

	if b.tokens < 1 {
		return false
	}
	b.tokens--
	return true
}

func min(a, b float64) float64 {
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
