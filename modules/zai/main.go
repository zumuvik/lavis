// Command zai is a Lavis external module (Module API v4) that reports
// remaining z.ai coding-plan quota. It answers the "ai" command, exposed as
// ,z.ai (module "z", command "ai") and ,z (default command), with the
// subcommands quota (default), usage and sub.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/user"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
)

const (
	protocolVersion = 4
	httpTimeout     = 2 * time.Second
	apiTimeFormat   = "2006-01-02 15:04:05"
	quotaPath       = "/api/monitor/usage/quota/limit"
	modelUsagePath  = "/api/monitor/usage/model-usage"
	subscriptionPth = "/api/biz/subscription/list"
	maxModelsShown  = 8
)

// apiBase is a variable so tests can point the module at a local server.
var apiBase = "https://api.z.ai"

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

// apiEnvelope holds the fields shared by every z.ai monitor/biz response.
type apiEnvelope struct {
	Code    int    `json:"code"`
	Msg     string `json:"msg"`
	Success bool   `json:"success"`
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
	apiEnvelope
	Data quotaPayload `json:"data"`
}

type modelSummary struct {
	ModelName   string `json:"modelName"`
	TotalTokens int64  `json:"totalTokens"`
}

// modelData carries per-model totals that are aligned with the requested
// window, unlike totalUsage.modelSummaryList which ignores startTime/endTime.
type modelData struct {
	ModelName   string `json:"modelName"`
	TotalTokens int64  `json:"totalTokens"`
}

type totalUsage struct {
	TotalModelCallCount int64          `json:"totalModelCallCount"`
	TotalTokensUsage    int64          `json:"totalTokensUsage"`
	ModelSummaryList    []modelSummary `json:"modelSummaryList"`
}

type usageResponse struct {
	apiEnvelope
	Data struct {
		TotalUsage    totalUsage  `json:"totalUsage"`
		ModelDataList []modelData `json:"modelDataList"`
	} `json:"data"`
}

// usageWindow is the window-aligned projection the reports render.
type usageWindow struct {
	Calls  int64
	Tokens int64
	Models []modelData
}

type subscription struct {
	ProductName   string  `json:"productName"`
	Status        string  `json:"status"`
	Valid         string  `json:"valid"`
	AutoRenew     int     `json:"autoRenew"`
	InitialPrice  float64 `json:"initialPrice"`
	StandardPrice float64 `json:"standardPrice"`
}

type subscriptionsResponse struct {
	apiEnvelope
	Data []subscription `json:"data"`
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
		base.Text = execute(req.Command, req.Arguments)
	default:
		base.Type = "error"
		base.Code = "UNKNOWN_TYPE"
		base.Message = "unknown request type"
	}
	return base
}

// execute never returns a module error: expected conditions (missing token,
// API failures) are reported as result text so the exact reason stays visible.
func execute(command, arguments string) string {
	if strings.ToLower(strings.TrimSpace(command)) != "ai" {
		return "❌ Неизвестная команда: " + command + ". Доступно: ai"
	}
	switch strings.ToLower(strings.TrimSpace(arguments)) {
	case "", "quota":
		return quotaReport()
	case "usage":
		return usageReport()
	case "sub":
		return subscriptionReport()
	default:
		return "❌ Неизвестный аргумент: " + arguments + ". Доступно: quota, usage, sub"
	}
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

// apiGet performs an authenticated GET against the z.ai monitor/biz API and
// decodes the body into out. Non-200 answers are turned into errors carrying
// the API's own message when one is present.
func apiGet(token, path string, query url.Values, out any) error {
	endpoint := apiBase + path
	if len(query) > 0 {
		endpoint += "?" + query.Encode()
	}
	req, err := http.NewRequest(http.MethodGet, endpoint, nil)
	if err != nil {
		return fmt.Errorf("запрос: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Accept", "application/json")
	client := &http.Client{Timeout: httpTimeout}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("сеть: %w", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return fmt.Errorf("ответ Z.AI API (HTTP %d): %w", resp.StatusCode, err)
	}
	if resp.StatusCode != http.StatusOK {
		var envelope apiEnvelope
		_ = json.Unmarshal(body, &envelope)
		return fmt.Errorf("Z.AI API: %s (HTTP %d)", envelope.Msg, resp.StatusCode)
	}
	if err := json.Unmarshal(body, out); err != nil {
		return fmt.Errorf("ответ Z.AI API (HTTP %d): %w", resp.StatusCode, err)
	}
	return nil
}

func quotaReport() string {
	token, err := loadToken()
	if err != nil {
		return "❌ " + err.Error()
	}
	var payload quotaResponse
	if err := apiGet(token, quotaPath, nil, &payload); err != nil {
		return "❌ " + err.Error()
	}
	if !payload.Success {
		return fmt.Sprintf("❌ Z.AI API: %s (HTTP %d)", payload.Msg, payload.Code)
	}
	if len(payload.Data.Limits) == 0 {
		return "❌ Z.AI API не вернул лимитов"
	}
	return formatQuota(payload.Data.Limits, payload.Data.Level)
}

func usageReport() string {
	token, err := loadToken()
	if err != nil {
		return "❌ " + err.Error()
	}
	now := time.Now().UTC()
	// Calendar-aligned windows are the only ones where the API returns
	// per-model totals that sum to the window total; sliding windows make
	// modelDataList diverge from totalUsage.
	todayStart := time.Date(now.Year(), now.Month(), now.Day(), 0, 0, 0, 0, time.UTC)
	dayEnd := todayStart.Add(24*time.Hour - time.Second)
	var b strings.Builder
	b.WriteString("📊 Z.AI — использование моделей\n")
	for _, window := range []struct {
		label string
		start time.Time
		end   time.Time
	}{{"сегодня", todayStart, dayEnd}, {"7д", todayStart.AddDate(0, 0, -6), dayEnd}} {
		usage, err := fetchUsage(token, window.start, window.end)
		if err != nil {
			return "❌ " + err.Error()
		}
		b.WriteString(formatUsageLine(window.label, usage))
	}
	return strings.TrimSuffix(b.String(), "\n")
}

func fetchUsage(token string, start, end time.Time) (usageWindow, error) {
	query := url.Values{}
	query.Set("startTime", start.UTC().Format(apiTimeFormat))
	query.Set("endTime", end.UTC().Format(apiTimeFormat))
	var payload usageResponse
	if err := apiGet(token, modelUsagePath, query, &payload); err != nil {
		return usageWindow{}, err
	}
	if !payload.Success {
		return usageWindow{}, fmt.Errorf("Z.AI API: %s (HTTP %d)", payload.Msg, payload.Code)
	}
	return usageWindow{
		Calls:  payload.Data.TotalUsage.TotalModelCallCount,
		Tokens: payload.Data.TotalUsage.TotalTokensUsage,
		Models: payload.Data.ModelDataList,
	}, nil
}

func subscriptionReport() string {
	token, err := loadToken()
	if err != nil {
		return "❌ " + err.Error()
	}
	var payload subscriptionsResponse
	if err := apiGet(token, subscriptionPth, nil, &payload); err != nil {
		return "❌ " + err.Error()
	}
	if !payload.Success {
		return fmt.Sprintf("❌ Z.AI API: %s (HTTP %d)", payload.Msg, payload.Code)
	}
	if len(payload.Data) == 0 {
		return "💳 Z.AI — активных подписок нет"
	}
	var b strings.Builder
	b.WriteString("💳 Z.AI — подписки\n")
	for _, sub := range payload.Data {
		fmt.Fprintf(&b, "• %s\n", formatSubscription(sub))
	}
	return strings.TrimSuffix(b.String(), "\n")
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
	return formatQuotaAt(limits, level, time.Now())
}

func formatQuotaAt(limits []quotaLimit, level string, now time.Time) string {
	var b strings.Builder
	b.WriteString("🪙 Z.AI")
	if level != "" {
		fmt.Fprintf(&b, " — план %s", level)
	}
	b.WriteString("\n")
	for _, limit := range limits {
		reset := formatResetTime(limit.NextResetTime)
		if countdown := formatDurationUntil(limit.NextResetTime, now); countdown != "" {
			reset += " (через " + countdown + ")"
		}
		fmt.Fprintf(&b, "• %s%s: %s / %s · %d%% %s · осталось %s · сброс %s\n",
			windowLabel(limit),
			limitSuffix(limit),
			formatInt(limit.CurrentValue),
			formatInt(limit.Usage),
			limit.Percentage,
			percentageBar(limit.Percentage),
			formatInt(limit.Remaining),
			reset,
		)
	}
	return strings.TrimSuffix(b.String(), "\n")
}

func formatDurationUntil(milliseconds int64, now time.Time) string {
	if milliseconds <= 0 {
		return ""
	}
	remaining := time.UnixMilli(milliseconds).Sub(now)
	if remaining <= 0 {
		return ""
	}
	return formatDuration(remaining)
}

func formatDuration(remaining time.Duration) string {
	minutes := int64(remaining / time.Minute)
	if days := minutes / (24 * 60); days > 0 {
		return fmt.Sprintf("%dд %dч", days, (minutes%(24*60))/60)
	}
	if hours := minutes / 60; hours > 0 {
		return fmt.Sprintf("%dч %dм", hours, minutes%60)
	}
	return fmt.Sprintf("%dм", minutes)
}

func formatUsageLine(label string, window usageWindow) string {
	var b strings.Builder
	fmt.Fprintf(&b, "%s: %s %s · %s %s\n",
		label,
		formatInt(window.Calls),
		plural(window.Calls, "вызов", "вызова", "вызовов"),
		formatInt(window.Tokens),
		plural(window.Tokens, "токен", "токена", "токенов"),
	)
	models := window.Models
	sort.Slice(models, func(i, j int) bool { return models[i].TotalTokens > models[j].TotalTokens })
	for i, model := range models {
		if i >= maxModelsShown {
			fmt.Fprintf(&b, "  • …и ещё %d %s\n", len(models)-i, plural(int64(len(models)-i), "модель", "модели", "моделей"))
			break
		}
		fmt.Fprintf(&b, "  • %s: %s\n", model.ModelName, formatInt(model.TotalTokens))
	}
	return b.String()
}

func formatSubscription(sub subscription) string {
	var b strings.Builder
	fmt.Fprintf(&b, "%s · %s · автопродление: %s", sub.ProductName, sub.Status, autoRenewLabel(sub.AutoRenew))
	if sub.Valid != "" {
		fmt.Fprintf(&b, " · %s", sub.Valid)
	}
	if sub.InitialPrice > 0 {
		fmt.Fprintf(&b, " · %.2f", sub.InitialPrice)
	}
	return b.String()
}

func autoRenewLabel(autoRenew int) string {
	if autoRenew == 1 {
		return "вкл"
	}
	return "выкл"
}

func plural(n int64, one, few, many string) string {
	mod10 := n % 10
	mod100 := n % 100
	if mod10 == 1 && mod100 != 11 {
		return one
	}
	if mod10 >= 2 && mod10 <= 4 && (mod100 < 12 || mod100 > 14) {
		return few
	}
	return many
}

func formatResetTime(milliseconds int64) string {
	if milliseconds <= 0 {
		return "неизвестно"
	}
	return time.UnixMilli(milliseconds).Local().Format("02.01 15:04")
}
