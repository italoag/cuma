package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/italoag/cuma/internal/api"
	"github.com/italoag/cuma/internal/config"
	"github.com/italoag/cuma/internal/hub"
	"github.com/italoag/cuma/internal/models"
	"github.com/italoag/cuma/internal/scanner"
	"github.com/italoag/cuma/internal/store"
)

// Version is set at build time via -ldflags.
var Version = "dev"

func main() {
	cfgFile := flag.String("config", "", "path to config file")
	flag.Parse()

	cfg, err := config.Load(*cfgFile)
	if err != nil {
		log.Fatalf("failed to load config: %v", err)
	}

	// Store
	db, err := store.NewSQLiteStore(cfg.Database)
	if err != nil {
		log.Fatalf("failed to open database: %v", err)
	}
	defer db.Close()

	// Hub
	h := hub.New()
	go h.Run()

	// Root context cancelled on shutdown signal
	rootCtx, rootCancel := context.WithCancel(context.Background())
	defer rootCancel()

	// Scanner orchestrator
	o := scanner.NewOrchestrator(cfg.Scanner, db, h)

	// Router
	router := api.NewRouter(cfg, db, o, h, Version)

	addr := fmt.Sprintf("%s:%d", cfg.Server.Host, cfg.Server.Port)
	srv := &http.Server{
		Addr:         addr,
		Handler:      router,
		ReadTimeout:  cfg.Server.ReadTimeout,
		WriteTimeout: cfg.Server.WriteTimeout,
	}

	// Auto-scan loop: respects rootCtx for graceful shutdown
	if cfg.Scanner.AutoScanInterval > 0 {
		go func() {
			select {
			case <-time.After(2 * time.Second): // wait for server to be ready
			case <-rootCtx.Done():
				return
			}
			for {
				scanCtx, scanCancel := context.WithTimeout(rootCtx, 5*time.Minute)
				_, _ = o.StartScan(scanCtx, models.ScanRequest{})
				scanCancel()

				select {
				case <-rootCtx.Done():
					return
				case <-time.After(cfg.Scanner.AutoScanInterval):
				}
			}
		}()
	}

	// Graceful shutdown
	quit := make(chan os.Signal, 1)
	signal.Notify(quit, syscall.SIGTERM, syscall.SIGINT)

	go func() {
		log.Printf("CUMA %s listening on %s", Version, addr)
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatalf("server error: %v", err)
		}
	}()

	<-quit
	log.Println("shutting down...")

	// Cancel root context first to stop auto-scan loop
	rootCancel()

	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), cfg.Server.ShutdownTimeout)
	defer shutdownCancel()

	if err := srv.Shutdown(shutdownCtx); err != nil {
		log.Printf("server shutdown error: %v", err)
	}
	log.Println("bye")
	os.Exit(0)
}
