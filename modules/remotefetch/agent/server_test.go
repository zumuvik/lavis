package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func newTestServer(t *testing.T) *server {
	t.Helper()
	return &server{
		token:   []byte("test-token-0123456789"),
		host:    "testhost",
		timeout: 10 * time.Second,
		slots:   make(chan struct{}, maxRuns),
		started: time.Now(),
	}
}

func request(t *testing.T, srv *server, method, path, token, body string) *httptest.ResponseRecorder {
	t.Helper()
	var reader *strings.Reader
	if body != "" {
		reader = strings.NewReader(body)
	}
	var req *http.Request
	if reader != nil {
		req = httptest.NewRequest(method, path, reader)
	} else {
		req = httptest.NewRequest(method, path, nil)
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	recorder := httptest.NewRecorder()
	srv.routes().ServeHTTP(recorder, req)
	return recorder
}

func TestEndpointsRequireTheBearerToken(t *testing.T) {
	srv := newTestServer(t)
	for _, probe := range []struct{ method, path, body string }{
		{"GET", "/v1/health", ""},
		{"POST", "/v1/fastfetch", `{"args":[]}`},
	} {
		for _, token := range []string{"", "wrong-token-0123456789"} {
			got := request(t, srv, probe.method, probe.path, token, probe.body)
			if got.Code != http.StatusUnauthorized {
				t.Errorf("%s %s with token %q = %d, want 401", probe.method, probe.path, token, got.Code)
			}
		}
	}
}

func TestHealthReportsHost(t *testing.T) {
	srv := newTestServer(t)
	got := request(t, srv, "GET", "/v1/health", string(srv.token), "")
	if got.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200: %s", got.Code, got.Body)
	}
	var payload healthResponse
	if err := json.Unmarshal(got.Body.Bytes(), &payload); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if !payload.OK || payload.Host != "testhost" {
		t.Fatalf("unexpected health payload: %#v", payload)
	}
}

func TestFastfetchRejectsPolicyViolationAndMalformedBody(t *testing.T) {
	srv := newTestServer(t)
	denied := request(t, srv, "POST", "/v1/fastfetch", string(srv.token), `{"args":["--file","/etc/shadow"]}`)
	if denied.Code != http.StatusBadRequest {
		t.Errorf("policy violation = %d, want 400", denied.Code)
	}
	malformed := request(t, srv, "POST", "/v1/fastfetch", string(srv.token), `{"args":`)
	if malformed.Code != http.StatusBadRequest {
		t.Errorf("malformed body = %d, want 400", malformed.Code)
	}
	unknown := request(t, srv, "POST", "/v1/fastfetch", string(srv.token), `{"args":[],"exec":"sh"}`)
	if unknown.Code != http.StatusBadRequest {
		t.Errorf("unknown field = %d, want 400", unknown.Code)
	}
}

func TestFastfetchReturnsOutput(t *testing.T) {
	if _, err := exec.LookPath("fastfetch"); err != nil {
		t.Skip("fastfetch is not installed")
	}
	srv := newTestServer(t)
	got := request(t, srv, "POST", "/v1/fastfetch", string(srv.token), `{"args":["--structure","OS"]}`)
	if got.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200: %s", got.Code, got.Body)
	}
	var payload fetchResponse
	if err := json.Unmarshal(got.Body.Bytes(), &payload); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if payload.Host != "testhost" || !strings.Contains(payload.Output, "OS") {
		t.Fatalf("unexpected payload: %#v", payload)
	}
}

func TestFastfetchRejectsBusyAgent(t *testing.T) {
	srv := newTestServer(t)
	for i := 0; i < maxRuns; i++ {
		srv.slots <- struct{}{}
	}
	got := request(t, srv, "POST", "/v1/fastfetch", string(srv.token), `{"args":[]}`)
	if got.Code != http.StatusTooManyRequests {
		t.Fatalf("status = %d, want 429", got.Code)
	}
}

func TestReadTokenRejectsWeakFiles(t *testing.T) {
	dir := t.TempDir()
	short := filepath.Join(dir, "short")
	if err := os.WriteFile(short, []byte("tiny"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := readToken(short); err == nil {
		t.Error("readToken accepted a token below the minimum length")
	}

	exposed := filepath.Join(dir, "exposed")
	if err := os.WriteFile(exposed, []byte("0123456789abcdefghij"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := readToken(exposed); err == nil {
		t.Error("readToken accepted a world-readable token file")
	}

	good := filepath.Join(dir, "good")
	if err := os.WriteFile(good, []byte(" 0123456789abcdefghij \n"), 0o600); err != nil {
		t.Fatal(err)
	}
	token, err := readToken(good)
	if err != nil {
		t.Fatalf("readToken: %v", err)
	}
	if string(token) != "0123456789abcdefghij" {
		t.Fatalf("token = %q", token)
	}
}
