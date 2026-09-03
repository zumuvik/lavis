// Command zai is a Lavis external module (Module API v4) that reports
// remaining z.ai coding-plan quota. It answers the "ai" command, exposed as
// ,z.ai (module "z", command "ai") and ,z (default command).
package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const (
	protocolVersion = 4
	httpTimeout     = 3 * time.Second
)

// quotaURL is a variable so tests can point the module at a local server.
var quotaURL = "https://api.z.ai/api/monitor/usage/quota/limit"

type request struct {
	ProtocolVersion int    `json:"protocol_version"`
	Type            string `json:"type"`
	RequestID       string `json:"request_id"`
	ModuleID        string `json:"module_id"`
	Command         string `json:"command"`
	Arguments       string `json:"arguments"`
}

type response struct {
	ProtocolVersion int    `json:"protocol_version"`
	Type            string `json:"type"`
	RequestID       string `json:"request_id"`
	ModuleID        string `json:"module_id,omitempty"`
	Text            string `json:"text,omitempty"`
	Code            string `json:"code,omitempty"`
	Message         string `json:"message,omitempty"`
}

type quotaLimit struct {
	Type          string `json:"type"`
	Unit          int    `json:"unit"`
	Number        int    `json:"number"`
	Usage         int64  `json:"usage"`
	CurrentValue  int64  `json:"currentValue"`
	Remaining     int64  `json:"remaining"`
	Percentage    int    `json:"percentage"`
	NextResetTime int64  `json:"nextResetTime"`
}

type quotaPayload struct {
	Limits []quotaLimit `json:"limits"`
	Level  string       `json:"level"`
}

type quotaResponse struct {
	Code    int          `json:"code"`
	Msg     string       `json:"msg"`
	Data    quotaPayload `json:"data"`
	Success bool         `json:"success"`
}

func main() {
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), 64*1024)
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	for scanner.Scan() {
		var req request
		if err := json.Unmarshal(scanner.Bytes(), &req); err != nil {
			continue
		}
		if err := encoder.Encode(handle(req)); err != nil {
			fmt.Fprintln(os.Stderr, err)
			return
		}
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func handle(req request) response {
	base := response{ProtocolVersion: protocolVersion, RequestID: req.RequestID}
	if req.ProtocolVersion != protocolVersion {
		base.Type = "error"
		base.Code = "PROTOCOL_VERSION"
		base.Message = "unsupported protocol version"
		return base
	}
	switch req.Type {
	case "initialize":
		base.Type = "initialized"
		base.ModuleID = req.ModuleID
	case "health":
		base.Type = "health"
	case "shutdown":
		os.Exit(0)
	case "execute":
		base.Type = "result"
		base.Text = execute(req.Command)
	default:
		base.Type = "error"
		base.Code = "UNKNOWN_TYPE"
		base.Message = "unknown request type"
	}
	return base
}

// execute never returns a module error: expected conditions (missing token,
// API failures) are reported as result text so the exact reason stays visible.
func execute(command string) string {
	if strings.ToLower(strings.TrimSpace(command)) != "ai" {
		return "❌ Неизвестная команда: " + command + ". Доступно: ai"
	}
	token, err := loadToken()
	if err != nil {
		return "❌ " + err.Error()
	}
	limits, level, err := fetchQuota(token)
	if err != nil {
		return "❌ " + err.Error()
	}
	return formatQuota(limits, level)
}

// tokenPath resolves the user's home directory without relying on $HOME:
// Lavis clears the environment of module processes, so when HOME is absent
// the passwd database is the remaining source of truth.
func tokenPath() (string, error) {
	if home := os.Getenv("HOME"); home != "" {
		return filepath.Join(home, ".env"), nil
	}
	current, err := user.Current()
	if err != nil {
		return "", fmt.Errorf("не удалось определить домашний каталог: %w", err)
	}
	return filepath.Join(current.HomeDir, ".env"), nil
}

func loadToken() (string, error) {
	path, err := tokenPath()
	if err != nil {
		return "", err
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("не удалось прочитать %s: %w", path, err)
	}
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, value, found := strings.Cut(line, "=")
		if !found || strings.TrimSpace(key) != "token" {
			continue
		}
		value = strings.Trim(strings.TrimSpace(value), `"'`)
		if value == "" {
			return "", fmt.Errorf("в %s ключ token пустой", path)
		}
		return value, nil
	}
	return "", fmt.Errorf("в %s нет ключа token", path)
}

func fetchQuota(token string) ([]quotaLimit, string, error) {
	req, err := http.NewRequest(http.MethodGet, quotaURL, nil)
	if err != nil {
		return nil, "", fmt.Errorf("запрос: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Accept", "application/json")
	client := &http.Client{Timeout: httpTimeout}
	resp, err := client.Do(req)
	if err != nil {
		return nil, "", fmt.Errorf("сеть: %w", err)
	}
	defer resp.Body.Close()
	var payload quotaResponse
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return nil, "", fmt.Errorf("ответ Z.AI API (HTTP %d): %w", resp.StatusCode, err)
	}
	if resp.StatusCode != http.StatusOK || !payload.Success {
		return nil, "", fmt.Errorf("Z.AI API: %s (HTTP %d)", payload.Msg, resp.StatusCode)
	}
	if len(payload.Data.Limits) == 0 {
		return nil, "", errors.New("Z.AI API не вернул лимитов")
	}
	return payload.Data.Limits, payload.Data.Level, nil
}

// unit codes are undocumented; mapping reverse-engineered from public z.ai
// clients (unit 1 = days, 3 = hours, 5 = months, 6 = weeks).
var unitSingular = map[int]string{1: "день", 3: "час", 5: "месяц", 6: "неделя"}
var unitShort = map[int]string{1: "дн", 3: "ч", 5: "мес", 6: "нед"}

func windowLabel(limit quotaLimit) string {
	if limit.Number <= 0 {
		return "окно"
	}
	if limit.Number == 1 {
		if name, ok := unitSingular[limit.Unit]; ok {
			return name
		}
		return "окно"
	}
	if suffix, ok := unitShort[limit.Unit]; ok {
		return fmt.Sprintf("%d %s", limit.Number, suffix)
	}
	return fmt.Sprintf("окно %d", limit.Number)
}

var limitTypeLabel = map[string]string{
	"CREDIT_LIMIT": "",
	"TOKENS_LIMIT": "токены",
	"TIME_LIMIT":   "время",
}

func limitSuffix(limit quotaLimit) string {
	label, known := limitTypeLabel[limit.Type]
	if !known {
		if limit.Type == "" {
			return ""
		}
		label = strings.ToLower(limit.Type)
	}
	if label == "" {
		return ""
	}
	return " (" + label + ")"
}

func percentageBar(percentage int) string {
	if percentage < 0 {
		percentage = 0
	}
	if percentage > 100 {
		percentage = 100
	}
	filled := percentage / 10
	var b strings.Builder
	for i := 0; i < 10; i++ {
		if i < filled {
			b.WriteRune('▰')
		} else {
			b.WriteRune('▱')
		}
	}
	return b.String()
}

func formatInt(value int64) string {
	digits := strconv.FormatInt(value, 10)
	var parts []string
	for len(digits) > 3 {
		parts = append([]string{digits[len(digits)-3:]}, parts...)
		digits = digits[:len(digits)-3]
	}
	return strings.Join(append([]string{digits}, parts...), " ")
}

func formatQuota(limits []quotaLimit, level string) string {
	var b strings.Builder
	b.WriteString("🪙 Z.AI")
	if level != "" {
		fmt.Fprintf(&b, " — план %s", level)
	}
	b.WriteString("\n")
	for _, limit := range limits {
		fmt.Fprintf(&b, "• %s%s: %s / %s · %d%% %s · осталось %s · сброс %s\n",
			windowLabel(limit),
			limitSuffix(limit),
			formatInt(limit.CurrentValue),
			formatInt(limit.Usage),
			limit.Percentage,
			percentageBar(limit.Percentage),
			formatInt(limit.Remaining),
			formatResetTime(limit.NextResetTime),
		)
	}
	return strings.TrimSuffix(b.String(), "\n")
}

func formatResetTime(milliseconds int64) string {
	if milliseconds <= 0 {
		return "неизвестно"
	}
	return time.UnixMilli(milliseconds).Local().Format("02.01 15:04")
}
