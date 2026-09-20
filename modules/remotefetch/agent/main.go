// Command lavis-fetchd answers fastfetch queries for a Lavis userbot running
// on a different machine. It is meant to listen on a private address (a
// Tailscale or VPN interface) and authenticates every request with a shared
// bearer token, so the userbot host reports this machine's fastfetch output
// without holding shell access to it.
package main

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"
)

const (
	defaultListen  = "127.0.0.1:8471"
	defaultTimeout = 2500 * time.Millisecond
	// maxRequestBytes only stops an oversized body from being buffered; the
	// argument bounds in policy.go are the real guard.
	maxRequestBytes = 16 * 1024
	maxOutputBytes  = 64 * 1024
	// maxRuns bounds concurrent fastfetch processes. Detection touches disks,
	// DBus and the network, so an unbounded queue would be a self-inflicted
	// load generator on a laptop.
	maxRuns       = 2
	minTokenBytes = 16
)

type server struct {
	token         []byte
	host          string
	timeout       time.Duration
	allowFileArgs bool
	slots         chan struct{}
	started       time.Time

	versionOnce sync.Once
	version     string
}

type fetchRequest struct {
	Args []string `json:"args"`
}

type fetchResponse struct {
	Output    string `json:"output"`
	Host      string `json:"host"`
	TookMS    int64  `json:"took_ms"`
	Truncated bool   `json:"truncated"`
}

type healthResponse struct {
	OK        bool   `json:"ok"`
	Host      string `json:"host"`
	Fastfetch string `json:"fastfetch"`
	UptimeS   int64  `json:"uptime_s"`
}

type errorResponse struct {
	Error string `json:"error"`
}

func main() {
	listen := flag.String("listen", defaultListen, "address to listen on; use a private interface address")
	tokenFile := flag.String("token-file", "", "file holding the shared bearer token (required)")
	timeout := flag.Duration("timeout", defaultTimeout, "maximum fastfetch run time")
	allowFileArgs := flag.Bool("allow-file-args", false, "permit fastfetch options that read local files")
	flag.Parse()

	if *tokenFile == "" {
		log.Fatal("lavis-fetchd: --token-file is required")
	}
	token, err := readToken(*tokenFile)
	if err != nil {
		log.Fatalf("lavis-fetchd: %v", err)
	}
	hostname, err := os.Hostname()
	if err != nil {
		hostname = "unknown"
	}

	srv := &server{
		token:         token,
		host:          hostname,
		timeout:       *timeout,
		allowFileArgs: *allowFileArgs,
		slots:         make(chan struct{}, maxRuns),
		started:       time.Now(),
	}
	httpServer := &http.Server{
		Addr:              *listen,
		Handler:           srv.routes(),
		ReadHeaderTimeout: 3 * time.Second,
		ReadTimeout:       5 * time.Second,
		// WriteTimeout has to outlast a full fastfetch run plus its kill grace.
		WriteTimeout:   30 * time.Second,
		IdleTimeout:    30 * time.Second,
		MaxHeaderBytes: 8 * 1024,
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := httpServer.Shutdown(shutdownCtx); err != nil {
			log.Printf("lavis-fetchd: shutdown: %v", err)
		}
	}()

	log.Printf("lavis-fetchd: listening on %s as %s (file args allowed: %t)", *listen, hostname, *allowFileArgs)
	if err := httpServer.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatalf("lavis-fetchd: %v", err)
	}
}

func (s *server) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /v1/fastfetch", s.handleFastfetch)
	mux.HandleFunc("GET /v1/health", s.handleHealth)
	return mux
}

// authorized compares the bearer token in constant time. Both endpoints are
// authenticated: an unauthenticated health probe would hand out the hostname
// and fastfetch version to anything that can reach the listener.
func (s *server) authorized(r *http.Request) bool {
	value, ok := strings.CutPrefix(r.Header.Get("Authorization"), "Bearer ")
	if !ok {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(strings.TrimSpace(value)), s.token) == 1
}

func (s *server) handleHealth(w http.ResponseWriter, r *http.Request) {
	if !s.authorized(r) {
		writeError(w, http.StatusUnauthorized, "invalid token")
		return
	}
	s.versionOnce.Do(func() {
		ctx, cancel := context.WithTimeout(r.Context(), s.timeout)
		defer cancel()
		s.version = fastfetchVersion(ctx)
	})
	writeJSON(w, http.StatusOK, healthResponse{
		OK:        true,
		Host:      s.host,
		Fastfetch: s.version,
		UptimeS:   int64(time.Since(s.started).Seconds()),
	})
}

func (s *server) handleFastfetch(w http.ResponseWriter, r *http.Request) {
	if !s.authorized(r) {
		writeError(w, http.StatusUnauthorized, "invalid token")
		return
	}
	var request fetchRequest
	decoder := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxRequestBytes))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&request); err != nil {
		writeError(w, http.StatusBadRequest, "malformed request body")
		return
	}
	if err := checkArgs(request.Args, s.allowFileArgs); err != nil {
		writeError(w, http.StatusBadRequest, err.Error())
		return
	}

	select {
	case s.slots <- struct{}{}:
		defer func() { <-s.slots }()
	default:
		writeError(w, http.StatusTooManyRequests, "another fastfetch run is in flight")
		return
	}

	ctx, cancel := context.WithTimeout(r.Context(), s.timeout)
	defer cancel()
	started := time.Now()
	output, truncated, err := runFastfetch(ctx, request.Args, maxOutputBytes)
	took := time.Since(started)

	switch {
	case errors.Is(err, errUnavailable):
		writeError(w, http.StatusServiceUnavailable, "fastfetch is not installed on this machine")
	case errors.Is(err, context.DeadlineExceeded):
		writeError(w, http.StatusGatewayTimeout, fmt.Sprintf("fastfetch timed out after %s", s.timeout))
	case err != nil:
		writeError(w, http.StatusBadGateway, err.Error())
	default:
		writeJSON(w, http.StatusOK, fetchResponse{
			Output:    output,
			Host:      s.host,
			TookMS:    took.Milliseconds(),
			Truncated: truncated,
		})
	}
}

func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	if err := json.NewEncoder(w).Encode(body); err != nil {
		log.Printf("lavis-fetchd: encode response: %v", err)
	}
}

func writeError(w http.ResponseWriter, status int, message string) {
	writeJSON(w, status, errorResponse{Error: message})
}

// readToken loads the shared secret. A world-readable token file is refused
// outright: every local account could otherwise query this machine.
func readToken(path string) ([]byte, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, fmt.Errorf("token file: %w", err)
	}
	if info.Mode().Perm()&0o004 != 0 {
		return nil, fmt.Errorf("token file %s is world-readable (%#o); chmod 600 it", path, info.Mode().Perm())
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("token file: %w", err)
	}
	token := strings.TrimSpace(string(data))
	if len(token) < minTokenBytes {
		return nil, fmt.Errorf("token in %s is shorter than %d bytes", path, minTokenBytes)
	}
	return []byte(token), nil
}
