package main

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestHandleInitializeEchoesModuleID(t *testing.T) {
	got := handle(request{ProtocolVersion: 6, Type: "initialize", RequestID: "1", ModuleID: "remotefetch"})
	if got.Type != "initialized" || got.ModuleID != "remotefetch" || got.RequestID != "1" {
		t.Fatalf("unexpected initialize response: %#v", got)
	}
}

func TestHandleHealthAndEvent(t *testing.T) {
	health := handle(request{ProtocolVersion: 6, Type: "health", RequestID: "4"})
	if health.Type != "health" || health.RequestID != "4" {
		t.Fatalf("unexpected health response: %#v", health)
	}
	event := handle(request{ProtocolVersion: 6, Type: "event", RequestID: "3"})
	if event.Type != "event_result" || event.Actions == nil || len(*event.Actions) != 0 {
		t.Fatalf("unexpected event response: %#v", event)
	}
	// An event_result without an actions array is a protocol violation, so the
	// empty slice must survive encoding rather than collapsing to null.
	encoded, err := json.Marshal(event)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if !strings.Contains(string(encoded), `"actions":[]`) {
		t.Fatalf("actions did not encode as an empty array: %s", encoded)
	}
}

func TestHandleRejectsForeignProtocolVersion(t *testing.T) {
	got := handle(request{ProtocolVersion: 5, Type: "initialize", RequestID: "1", ModuleID: "remotefetch"})
	if got.Type != "error" || got.Code != "PROTOCOL_VERSION" {
		t.Fatalf("unexpected response: %#v", got)
	}
}

func TestClipKeepsShortTextAndBoundsLongText(t *testing.T) {
	short := "OS: NixOS\nCPU: i7"
	if clip(short) != short {
		t.Fatalf("clip altered short text")
	}
	long := strings.Repeat("a", maxReplyUnits*2)
	clipped := clip(long)
	if utf16Len(clipped) > maxReplyUnits {
		t.Fatalf("clipped text is %d units, limit %d", utf16Len(clipped), maxReplyUnits)
	}
	if !strings.HasSuffix(clipped, "… вывод обрезан") {
		t.Fatalf("clipped text lost its marker: %q", clipped[len(clipped)-40:])
	}
}

func TestClipCountsSurrogatePairs(t *testing.T) {
	// Non-BMP runes cost two UTF-16 units; counting runes would overshoot the
	// host's rendering budget by a factor of two.
	text := strings.Repeat("🦀", maxReplyUnits)
	if utf16Len(clip(text)) > maxReplyUnits {
		t.Fatalf("clip exceeded the budget on non-BMP input")
	}
}

func TestConfigResolveValidatesURLAndToken(t *testing.T) {
	cases := []struct {
		name    string
		cfg     config
		wantErr bool
	}{
		{"ok", config{URL: "http://100.120.95.96:8471", Token: "0123456789abcdef"}, false},
		{"trailing slash", config{URL: "http://host:8471/", Token: "secret"}, false},
		{"no url", config{Token: "secret"}, true},
		{"no token", config{URL: "http://host:8471"}, true},
		{"bad scheme", config{URL: "ftp://host", Token: "secret"}, true},
		{"no host", config{URL: "http://", Token: "secret"}, true},
	}
	for _, tc := range cases {
		cfg := tc.cfg
		err := cfg.resolve()
		if tc.wantErr != (err != nil) {
			t.Errorf("%s: resolve() error = %v, wantErr %t", tc.name, err, tc.wantErr)
		}
		if err == nil && strings.HasSuffix(cfg.URL, "/") {
			t.Errorf("%s: trailing slash survived: %q", tc.name, cfg.URL)
		}
	}
}

func TestConfigResolveReadsTokenFile(t *testing.T) {
	path := t.TempDir() + "/token"
	if err := writeFile(path, "  file-token\n"); err != nil {
		t.Fatalf("write: %v", err)
	}
	cfg := config{URL: "http://host:8471", TokenFile: path}
	if err := cfg.resolve(); err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if cfg.Token != "file-token" {
		t.Fatalf("token = %q, want %q", cfg.Token, "file-token")
	}
}

func TestDisplayNamePrefersLabelThenReportedHost(t *testing.T) {
	labelled := &config{Label: "xpert"}
	if got := labelled.displayName("other"); got != "xpert" {
		t.Errorf("displayName with label = %q", got)
	}
	bare := &config{}
	if got := bare.displayName("xpert"); got != "xpert" {
		t.Errorf("displayName with reported host = %q", got)
	}
	if got := bare.displayName(""); got != "хост" {
		t.Errorf("displayName fallback = %q", got)
	}
}

func TestConfigResolveRejectsHeaderUnsafeTokens(t *testing.T) {
	// A token that cannot be an Authorization header value must fail here, not
	// inside net/http where it would look like an unreachable agent.
	for _, token := range []string{"bad\ntoken", "bad token", "токен"} {
		cfg := config{URL: "http://host:8471", Token: token}
		if err := cfg.resolve(); err == nil {
			t.Errorf("resolve() accepted token %q", token)
		}
	}
	cfg := config{URL: "http://host:8471", Token: "Zm9vYmFy+/=abcDEF123"}
	if err := cfg.resolve(); err != nil {
		t.Errorf("resolve() rejected a base64 token: %v", err)
	}
}
