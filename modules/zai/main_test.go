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
)

func TestLoadToken(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("HOME", dir)
	path := filepath.Join(dir, ".env")
	content := "# comment\ntoken =  \"abc def\"\nother=1\n"
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	token, err := loadToken()
	if err != nil {
		t.Fatalf("loadToken: %v", err)
	}
	if token != "abc def" {
		t.Fatalf("token = %q", token)
	}
}

func TestLoadTokenMissingKey(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("HOME", dir)
	if err := os.WriteFile(filepath.Join(dir, ".env"), []byte("other=1\n"), 0o600); err != nil {
		t.Fatal(err)
	}
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
		0:     "0",
		406:   "406",
		2000:  "2 000",
		10000: "10 000",
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

func TestFormatQuota(t *testing.T) {
	limits := []quotaLimit{
		{Type: "CREDIT_LIMIT", Unit: 3, Number: 5, Usage: 2000, CurrentValue: 406, Remaining: 1593, Percentage: 20},
		{Type: "CREDIT_LIMIT", Unit: 6, Number: 1, Usage: 10000, CurrentValue: 8659, Remaining: 1340, Percentage: 86},
	}
	text := formatQuota(limits, "lite")
	for _, want := range []string{"план lite", "5 ч: 406 / 2 000", "неделя: 8 659 / 10 000", "осталось 1 593", "осталось 1 340"} {
		if !strings.Contains(text, want) {
			t.Errorf("formatQuota missing %q in:\n%s", want, text)
		}
	}
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

func TestHandleExecuteUnknownCommand(t *testing.T) {
	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r4", Command: "bogus"})
	if resp.Type != "result" || !strings.Contains(resp.Text, "Неизвестная команда") {
		t.Fatalf("unknown command handling: %+v", resp)
	}
}

func TestHandleExecuteFetchesQuota(t *testing.T) {
	original := quotaURL
	t.Cleanup(func() { quotaURL = original })
	var gotAuth string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"code":200,"msg":"Operation successful","data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":2000,"currentValue":406,"remaining":1593,"percentage":20,"nextResetTime":1788447511263}],"level":"lite"},"success":true}`)
	}))
	t.Cleanup(server.Close)
	quotaURL = server.URL

	dir := t.TempDir()
	t.Setenv("HOME", dir)
	if err := os.WriteFile(filepath.Join(dir, ".env"), []byte("token=secret\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r5", Command: "ai"})
	if resp.Type != "result" {
		t.Fatalf("execute type: %+v", resp)
	}
	if gotAuth != "Bearer secret" {
		t.Fatalf("Authorization header = %q", gotAuth)
	}
	if !strings.Contains(resp.Text, "406 / 2 000") || !strings.Contains(resp.Text, "план lite") {
		t.Fatalf("execute text:\n%s", resp.Text)
	}
}

func TestHandleExecuteReportsAPIText(t *testing.T) {
	original := quotaURL
	t.Cleanup(func() { quotaURL = original })
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		fmt.Fprint(w, `{"code":401,"msg":"token expired or incorrect","success":false}`)
	}))
	t.Cleanup(server.Close)
	quotaURL = server.URL

	dir := t.TempDir()
	t.Setenv("HOME", dir)
	if err := os.WriteFile(filepath.Join(dir, ".env"), []byte("token=bad\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	resp := handle(request{ProtocolVersion: 4, Type: "execute", RequestID: "r6", Command: "ai"})
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
