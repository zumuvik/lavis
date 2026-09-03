package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func writeTokenFile(t *testing.T, content string) {
	t.Helper()
	dir := t.TempDir()
	t.Setenv("HOME", dir)
	if err := os.WriteFile(filepath.Join(dir, ".env"), []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestLoadToken(t *testing.T) {
	writeTokenFile(t, "# comment\ntoken =  \"abc def\"\nother=1\n")
	token, err := loadToken()
	if err != nil {
		t.Fatalf("loadToken: %v", err)
	}
	if token != "abc def" {
		t.Fatalf("token = %q", token)
	}
}

func TestLoadTokenMissingKey(t *testing.T) {
	writeTokenFile(t, "other=1\n")
	if _, err := loadToken(); err == nil {
		t.Fatal("expected error for missing token key")
	}
}

func TestLoadTokenMissingFile(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	if _, err := loadToken(); err == nil {
		t.Fatal("expected error for missing .env")
	}
}

func TestWindowLabel(t *testing.T) {
	cases := []struct {
		limit quotaLimit
		want  string
	}{
		{quotaLimit{Unit: 3, Number: 5}, "5 ч"},
		{quotaLimit{Unit: 6, Number: 1}, "неделя"},
		{quotaLimit{Unit: 6, Number: 7}, "7 нед"},
		{quotaLimit{Unit: 1, Number: 30}, "30 дн"},
		{quotaLimit{Unit: 5, Number: 1}, "месяц"},
		{quotaLimit{Unit: 9, Number: 2}, "окно 2"},
	}
	for _, tc := range cases {
		if got := windowLabel(tc.limit); got != tc.want {
			t.Errorf("windowLabel(%+v) = %q, want %q", tc.limit, got, tc.want)
		}
	}
}

func TestLimitSuffix(t *testing.T) {
	if got := limitSuffix(quotaLimit{Type: "CREDIT_LIMIT"}); got != "" {
		t.Errorf("credit suffix = %q", got)
	}
	if got := limitSuffix(quotaLimit{Type: "TOKENS_LIMIT"}); got != " (токены)" {
		t.Errorf("tokens suffix = %q", got)
	}
	if got := limitSuffix(quotaLimit{Type: "WEIRD"}); got != " (weird)" {
		t.Errorf("unknown suffix = %q", got)
	}
}

func TestFormatInt(t *testing.T) {
	cases := map[int64]string{
		0:        "0",
		406:      "406",
		2000:     "2 000",
		10000:    "10 000",
		98457618: "98 457 618",
	}
	for value, want := range cases {
		if got := formatInt(value); got != want {
			t.Errorf("formatInt(%d) = %q, want %q", value, got, want)
		}
	}
}

func TestPercentageBar(t *testing.T) {
	cases := map[int]string{
		-5:  "▱▱▱▱▱▱▱▱▱▱",
		0:   "▱▱▱▱▱▱▱▱▱▱",
		20:  "▰▰▱▱▱▱▱▱▱▱",
		86:  "▰▰▰▰▰▰▰▰▱▱",
		100: "▰▰▰▰▰▰▰▰▰▰",
		150: "▰▰▰▰▰▰▰▰▰▰",
	}
	for pct, want := range cases {
		if got := percentageBar(pct); got != want {
			t.Errorf("percentageBar(%d) = %q, want %q", pct, got, want)
		}
	}
}

func TestPlural(t *testing.T) {
	cases := []struct {
		n    int64
		want string
	}{
		{1, "вызов"},
		{2, "вызова"},
		{5, "вызовов"},
		{11, "вызовов"},
		{21, "вызов"},
		{22, "вызова"},
		{111, "вызовов"},
	}
	for _, tc := range cases {
		if got := plural(tc.n, "вызов", "вызова", "вызовов"); got != tc.want {
			t.Errorf("plural(%d) = %q, want %q", tc.n, got, tc.want)
		}
	}
}

func TestFormatDuration(t *testing.T) {
	cases := map[time.Duration]string{
		45 * time.Minute:      "45м",
		90 * time.Minute:      "1ч 30м",
		5 * time.Hour:         "5ч 0м",
		27 * time.Hour:        "1д 3ч",
		3 * 24 * time.Hour:    "3д 0ч",
	}
	for d, want := range cases {
		if got := formatDuration(d); got != want {
			t.Errorf("formatDuration(%v) = %q, want %q", d, got, want)
		}
	}
}

func TestFormatQuotaAt(t *testing.T) {
	now := time.Date(2026, 9, 3, 12, 0, 0, 0, time.UTC)
	limits := []quotaLimit{
		{Type: "CREDIT_LIMIT", Unit: 3, Number: 5, Usage: 2000, CurrentValue: 406, Remaining: 1593, Percentage: 20, NextResetTime: now.Add(90 * time.Minute).UnixMilli()},
		{Type: "CREDIT_LIMIT", Unit: 6, Number: 1, Usage: 10000, CurrentValue: 8659, Remaining: 1340, Percentage: 86, NextResetTime: now.Add(30 * time.Minute).UnixMilli()},
		{Type: "CREDIT_LIMIT", Unit: 3, Number: 5, Usage: 2000, CurrentValue: 406, Remaining: 1593, Percentage: 20, NextResetTime: now.Add(-time.Minute).UnixMilli()},
	}
	text := formatQuotaAt(limits, "lite", now)
	for _, want := range []string{
		"план lite",
		"5 ч: 406 / 2 000",
		"осталось 1 593",
		"(через 1ч 30м)",
		"(через 30м)",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("formatQuotaAt missing %q in:\n%s", want, text)
		}
	}
	if strings.Count(text, "(через") != 2 {
		t.Errorf("expected countdown only for future resets:\n%s", text)
	}
}

func TestFormatUsageLine(t *testing.T) {
	window := usageWindow{
		Calls:  528,
		Tokens: 98457618,
		Models: []modelData{
			{ModelName: "GLM-5.2", TotalTokens: 500},
			{ModelName: "GLM-5.3-Flash", TotalTokens: 98457118},
		},
	}
	text := formatUsageLine("24ч", window)
	for _, want := range []string{
		"24ч: 528 вызовов · 98 457 618 токенов",
		"GLM-5.3-Flash: 98 457 118",
		"GLM-5.2: 500",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("formatUsageLine missing %q in:\n%s", want, text)
		}
	}
	if strings.Index(text, "GLM-5.3-Flash") > strings.Index(text, "GLM-5.2") {
		t.Errorf("models not sorted by tokens desc:\n%s", text)
	}
}

func TestFormatSubscription(t *testing.T) {
	sub := subscription{
		ProductName:  "GLM Coding Lite",
		Status:       "VALID",
		Valid:        "2026-09-30 22:54:22-2026-10-30 22:54:22",
		AutoRenew:    1,
		InitialPrice: 18,
	}
	text := formatSubscription(sub)
	for _, want := range []string{
		"GLM Coding Lite · VALID · автопродление: вкл",
		"2026-09-30 22:54:22-2026-10-30 22:54:22",
		"· 18.00",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("formatSubscription missing %q in:\n%s", want, text)
		}
	}
	if got := formatSubscription(subscription{ProductName: "X", AutoRenew: 0}); !strings.Contains(got, "автопродление: выкл") {
		t.Errorf("autoRenew off = %q", got)
	}
}

// stubAPI starts a test server that answers all three z.ai endpoints and
// points apiBase at it. It returns the queries received for model-usage.
func stubAPI(t *testing.T, quotaBody string) map[string][]string {
	t.Helper()
	original := apiBase
	t.Cleanup(func() { apiBase = original })
	queries := make(map[string][]string)
	mux := http.NewServeMux()
	mux.HandleFunc(quotaPath, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, quotaBody)
	})
	mux.HandleFunc(modelUsagePath, func(w http.ResponseWriter, r *http.Request) {
		queries["startTime"] = append(queries["startTime"], r.URL.Query().Get("startTime"))
		queries["endTime"] = append(queries["endTime"], r.URL.Query().Get("endTime"))
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"code":200,"msg":"Operation successful","success":true,"data":{"totalUsage":{"totalModelCallCount":528,"totalTokensUsage":98457618},"modelDataList":[{"modelName":"GLM-5.3-Flash","totalTokens":98457618}]}}`)
	})
	mux.HandleFunc(subscriptionPth, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"code":200,"msg":"Operation successful","success":true,"data":[{"productName":"GLM Coding Lite","status":"VALID","valid":"2026-09-30 22:54:22-2026-10-30 22:54:22","autoRenew":1,"initialPrice":18,"standardPrice":18}]}`)
	})
	server := httptest.NewServer(mux)
	t.Cleanup(server.Close)
	apiBase = server.URL
	return queries
}

func TestHandleProtocol(t *testing.T) {
	resp := handle(request{ProtocolVersion: 3, Type: "initialize", RequestID: "r1"})
	if resp.Type != "error" || resp.Code != "PROTOCOL_VERSION" {
		t.Fatalf("version mismatch handling: %+v", resp)
	}

	resp = handle(request{ProtocolVersion: 4, Type: "initialize", RequestID: "r2", ModuleID: "z"})
	if resp.Type != "initialized" || resp.RequestID != "r2" || resp.ModuleID != "z" {
		t.Fatalf("initialize handling: %+v", resp)
	}

	resp = handle(request{ProtocolVersion: 4, Type: "health", RequestID: "r3"})
	if resp.Type != "health" {
		t.Fatalf("health handling: %+v", resp)
	}
}

func TestExecuteDispatch(t *testing.T) {
	if text := execute("bogus", ""); !strings.Contains(text, "Неизвестная команда") {
		t.Fatalf("unknown command handling:\n%s", text)
	}
	if text := execute("ai", "bogus"); !strings.Contains(text, "Неизвестный аргумент") {
		t.Fatalf("unknown argument handling:\n%s", text)
	}
	if text := execute("ai", "BOGUS"); !strings.Contains(text, "Неизвестный аргумент") {
		t.Fatalf("argument case handling:\n%s", text)
	}
}

func TestHandleExecuteFetchesQuota(t *testing.T) {
	stubAPI(t, `{"code":200,"msg":"Operation successful","data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":2000,"currentValue":406,"remaining":1593,"percentage":20,"nextResetTime":1788447511263}],"level":"lite"},"success":true}`)
	writeTokenFile(t, "token=secret\n")

	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r5", Command: "ai"})
	if resp.Type != "result" {
		t.Fatalf("execute type: %+v", resp)
	}
	if !strings.Contains(resp.Text, "406 / 2 000") || !strings.Contains(resp.Text, "план lite") {
		t.Fatalf("execute text:\n%s", resp.Text)
	}
}

func TestHandleExecuteUsage(t *testing.T) {
	queries := stubAPI(t, `{}`)
	writeTokenFile(t, "token=secret\n")

	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r6", Command: "ai", Arguments: "usage"})
	if resp.Type != "result" {
		t.Fatalf("execute type: %+v", resp)
	}
	if len(queries["startTime"]) != 2 || len(queries["endTime"]) != 2 {
		t.Fatalf("expected two usage windows, queries: %v", queries)
	}
	for _, start := range queries["startTime"] {
		if len(start) != len(apiTimeFormat) {
			t.Fatalf("startTime format: %q", start)
		}
	}
	for _, want := range []string{"528 вызовов", "98 457 618", "GLM-5.3-Flash: 98 457 618", "сегодня:", "7д:"} {
		if !strings.Contains(resp.Text, want) {
			t.Fatalf("usage text missing %q:\n%s", want, resp.Text)
		}
	}
}

func TestHandleExecuteSub(t *testing.T) {
	stubAPI(t, `{}`)
	writeTokenFile(t, "token=secret\n")

	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r7", Command: "ai", Arguments: "sub"})
	if resp.Type != "result" {
		t.Fatalf("execute type: %+v", resp)
	}
	for _, want := range []string{"GLM Coding Lite · VALID · автопродление: вкл", "2026-09-30 22:54:22-2026-10-30 22:54:22", "· 18.00"} {
		if !strings.Contains(resp.Text, want) {
			t.Fatalf("sub text missing %q:\n%s", want, resp.Text)
		}
	}
}

func TestHandleExecuteReportsAPIText(t *testing.T) {
	original := apiBase
	t.Cleanup(func() { apiBase = original })
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		fmt.Fprint(w, `{"code":401,"msg":"token expired or incorrect","success":false}`)
	}))
	t.Cleanup(server.Close)
	apiBase = server.URL
	writeTokenFile(t, "token=bad\n")

	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r8", Command: "ai"})
	if resp.Type != "result" {
		t.Fatalf("execute type: %+v", resp)
	}
	if !strings.Contains(resp.Text, "token expired or incorrect") {
		t.Fatalf("execute text:\n%s", resp.Text)
	}
}

func TestResponseEncodesCleanText(t *testing.T) {
	data, err := json.Marshal(response{ProtocolVersion: 4, Type: "result", RequestID: "r", Text: "a · b — c"})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(data), "\\u") {
		t.Fatalf("unexpected HTML escaping: %s", data)
	}
}
