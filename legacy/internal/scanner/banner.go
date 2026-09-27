package scanner

import (
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

type BannerGrabber struct {
	timeout time.Duration
}

func NewBannerGrabber(timeout time.Duration) *BannerGrabber {
	return &BannerGrabber{timeout: timeout}
}

type BannerResult struct {
	IP         string
	Port       int
	ServerHeader string
	Body       string
	StatusCode int
}

func (b *BannerGrabber) Grab(ip string, port int) (*BannerResult, error) {
	scheme := "http"
	if port == 443 || port == 8443 {
		scheme = "https"
	}
	url := fmt.Sprintf("%s://%s:%d/", scheme, ip, port)

	client := &http.Client{
		Timeout: b.timeout,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
		Transport: &http.Transport{
			TLSClientConfig: insecureTLS(),
		},
	}

	req, err := http.NewRequest("GET", url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "CUMA-IoT-Scanner/1.0")

	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	bodyBytes, _ := io.ReadAll(io.LimitReader(resp.Body, 512))
	body := strings.ToLower(string(bodyBytes))

	return &BannerResult{
		IP:           ip,
		Port:         port,
		ServerHeader: resp.Header.Get("Server"),
		Body:         body,
		StatusCode:   resp.StatusCode,
	}, nil
}

func (b *BannerResult) CombinedText() string {
	return strings.ToLower(b.ServerHeader + " " + b.Body)
}
