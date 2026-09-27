package config

import (
	"strings"
	"time"

	"github.com/joho/godotenv"
	"github.com/spf13/viper"
)

type Config struct {
	Server   ServerConfig   `mapstructure:"server"`
	Scanner  ScannerConfig  `mapstructure:"scanner"`
	Auth     AuthConfig     `mapstructure:"auth"`
	Database DatabaseConfig `mapstructure:"database"`
	Log      LogConfig      `mapstructure:"log"`
}

type ServerConfig struct {
	Host            string        `mapstructure:"host"`
	Port            int           `mapstructure:"port"`
	ReadTimeout     time.Duration `mapstructure:"read_timeout"`
	WriteTimeout    time.Duration `mapstructure:"write_timeout"`
	ShutdownTimeout time.Duration `mapstructure:"shutdown_timeout"`
}

type ScannerConfig struct {
	DefaultInterface string        `mapstructure:"default_interface"`
	ARPTimeout       time.Duration `mapstructure:"arp_timeout"`
	MDNSTimeout      time.Duration `mapstructure:"mdns_timeout"`
	SSDPTimeout      time.Duration `mapstructure:"ssdp_timeout"`
	PortScanTimeout  time.Duration `mapstructure:"port_scan_timeout"`
	BannerTimeout    time.Duration `mapstructure:"banner_timeout"`
	PortScanWorkers  int           `mapstructure:"port_scan_workers"`
	TargetPorts      []int         `mapstructure:"target_ports"`
	AutoScanInterval time.Duration `mapstructure:"auto_scan_interval"`
}

type AuthConfig struct {
	Mode          string        `mapstructure:"mode"`
	JWTSecret     string        `mapstructure:"jwt_secret"`
	APIKeys       []string      `mapstructure:"api_keys"`
	TokenTTL      time.Duration `mapstructure:"token_ttl"`
	AdminUsername string        `mapstructure:"admin_username"`
	AdminPassword string        `mapstructure:"admin_password"`
}

type DatabaseConfig struct {
	Driver string `mapstructure:"driver"`
	DSN    string `mapstructure:"dsn"`
}

type LogConfig struct {
	Level  string `mapstructure:"level"`
	Format string `mapstructure:"format"`
}

func Load(cfgFile string) (*Config, error) {
	_ = godotenv.Load()

	v := viper.New()

	if cfgFile != "" {
		v.SetConfigFile(cfgFile)
	} else {
		v.AddConfigPath("configs")
		v.AddConfigPath(".")
		v.SetConfigName("config")
		v.SetConfigType("yaml")
	}

	v.SetEnvPrefix("CUMA")
	v.SetEnvKeyReplacer(strings.NewReplacer(".", "_"))
	v.AutomaticEnv()

	setDefaults(v)

	if err := v.ReadInConfig(); err != nil {
		if _, ok := err.(viper.ConfigFileNotFoundError); !ok {
			return nil, err
		}
	}

	var cfg Config
	if err := v.Unmarshal(&cfg); err != nil {
		return nil, err
	}

	return &cfg, nil
}

func setDefaults(v *viper.Viper) {
	v.SetDefault("server.host", "0.0.0.0")
	v.SetDefault("server.port", 8080)
	v.SetDefault("server.read_timeout", "30s")
	v.SetDefault("server.write_timeout", "30s")
	v.SetDefault("server.shutdown_timeout", "10s")

	v.SetDefault("scanner.arp_timeout", "5s")
	v.SetDefault("scanner.mdns_timeout", "10s")
	v.SetDefault("scanner.ssdp_timeout", "10s")
	v.SetDefault("scanner.port_scan_timeout", "2s")
	v.SetDefault("scanner.banner_timeout", "5s")
	v.SetDefault("scanner.port_scan_workers", 20)
	v.SetDefault("scanner.target_ports", []int{80, 443, 1883, 5683, 8080, 8443, 8883, 5000, 9000})
	v.SetDefault("scanner.auto_scan_interval", "0s")

	v.SetDefault("auth.mode", "both")
	v.SetDefault("auth.jwt_secret", "change-me-in-production")
	v.SetDefault("auth.token_ttl", "24h")
	v.SetDefault("auth.admin_username", "admin")
	v.SetDefault("auth.admin_password", "change-me")

	v.SetDefault("database.driver", "sqlite")
	v.SetDefault("database.dsn", "./cuma.db")

	v.SetDefault("log.level", "info")
	v.SetDefault("log.format", "json")
}
