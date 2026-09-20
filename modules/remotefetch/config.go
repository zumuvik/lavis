package main

import (
	"encoding/json"
	"fmt"
	"net/url"
	"os"
	"os/user"
	"path/filepath"
	"strings"
)

const maxConfigBytes = 16 * 1024

type config struct {
	URL       string `json:"url"`
	Token     string `json:"token"`
	TokenFile string `json:"token_file"`
	Label     string `json:"label"`
}

// displayName prefers the operator's own label, falls back to the hostname the
// agent reported, and finally to a generic word, so every message names a
// machine even when the agent never answered.
func (c *config) displayName(reported string) string {
	switch {
	case c.Label != "":
		return c.Label
	case reported != "":
		return reported
	default:
		return "хост"
	}
}

// configPaths lists where the module looks for its settings. Lavis starts
// modules with a cleared environment, so $HOME is absent and the per-module
// state directory is the only location the host guarantees.
func configPaths() []string {
	var paths []string
	if dir := os.Getenv("LAVIS_MODULE_STATE_DIR"); dir != "" {
		paths = append(paths, filepath.Join(dir, "config.json"))
	}
	if home := homeDir(); home != "" {
		paths = append(paths, filepath.Join(home, ".config", "lavis", "remotefetch.json"))
	}
	return paths
}

func homeDir() string {
	if home := os.Getenv("HOME"); home != "" {
		return home
	}
	if current, err := user.Current(); err == nil {
		return current.HomeDir
	}
	return ""
}

func loadConfig() (*config, error) {
	paths := configPaths()
	for _, path := range paths {
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		if len(data) > maxConfigBytes {
			return nil, fmt.Errorf("Конфиг %s слишком большой", path)
		}
		cfg := &config{}
		if err := json.Unmarshal(data, cfg); err != nil {
			return nil, fmt.Errorf("Конфиг %s не разбирается: %v", path, err)
		}
		if err := cfg.resolve(); err != nil {
			return nil, fmt.Errorf("Конфиг %s: %v", path, err)
		}
		return cfg, nil
	}
	if len(paths) == 0 {
		return nil, fmt.Errorf("Негде искать конфиг: нет ни LAVIS_MODULE_STATE_DIR, ни домашнего каталога")
	}
	return nil, fmt.Errorf("Нет конфига. Создай %s с полями url и token", paths[0])
}

func (c *config) resolve() error {
	c.URL = strings.TrimRight(strings.TrimSpace(c.URL), "/")
	if c.URL == "" {
		return fmt.Errorf("не задан url")
	}
	parsed, err := url.Parse(c.URL)
	if err != nil || parsed.Host == "" || (parsed.Scheme != "http" && parsed.Scheme != "https") {
		return fmt.Errorf("url должен быть вида http://host:port")
	}
	if c.Token == "" && c.TokenFile != "" {
		data, err := os.ReadFile(c.TokenFile)
		if err != nil {
			return fmt.Errorf("не прочитать token_file %s", c.TokenFile)
		}
		c.Token = string(data)
	}
	c.Token = strings.TrimSpace(c.Token)
	if c.Token == "" {
		return fmt.Errorf("не задан token (или token_file)")
	}
	// The token is sent as an Authorization header value. Anything outside
	// printable ASCII fails inside net/http at request time, which surfaces as
	// an unreachable agent instead of naming the real problem.
	for _, r := range c.Token {
		if r < 0x21 || r > 0x7e {
			return fmt.Errorf("token должен состоять из печатаемых ASCII-символов без пробелов")
		}
	}
	c.Label = strings.TrimSpace(c.Label)
	return nil
}
