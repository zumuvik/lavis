package main

import (
	"context"
	"errors"
	"os/exec"
	"strings"
	"testing"
	"time"
)

func TestSanitizeStripsEscapesAndControlCharacters(t *testing.T) {
	cases := []struct {
		name  string
		input string
		want  string
	}{
		{"csi color", "\x1b[31mOS\x1b[0m: NixOS", "OS: NixOS"},
		{"osc title", "\x1b]0;title\x07OS", "OS"},
		{"osc st terminated", "\x1b]0;title\x1b\\OS", "OS"},
		{"dcs", "\x1bPq#0\x1b\\OS", "OS"},
		{"truncated csi", "OS\x1b[3", "OS"},
		{"crlf", "a\r\nb", "a\nb"},
		{"tab", "a\tb", "a    b"},
		{"trailing space", "a   \nb  ", "a\nb"},
		{"bidi override", "OS: \u202eNixOS", "OS: NixOS"},
		{"nul", "O\x00S", "OS"},
		{"trailing newlines", "OS\n\n\n", "OS"},
	}
	for _, tc := range cases {
		if got := sanitize([]byte(tc.input)); got != tc.want {
			t.Errorf("%s: sanitize(%q) = %q, want %q", tc.name, tc.input, got, tc.want)
		}
	}
}

func TestCappedBufferTruncatesWithoutShortWrites(t *testing.T) {
	buffer := &cappedBuffer{limit: 4}
	n, err := buffer.Write([]byte("abcdef"))
	if err != nil || n != 6 {
		t.Fatalf("Write = (%d, %v), want (6, nil)", n, err)
	}
	if string(buffer.Bytes()) != "abcd" || !buffer.truncated {
		t.Fatalf("buffer = %q truncated=%t", buffer.Bytes(), buffer.truncated)
	}
	if n, err := buffer.Write([]byte("gh")); err != nil || n != 2 {
		t.Fatalf("Write after fill = (%d, %v), want (2, nil)", n, err)
	}
}

func TestRunFastfetchReportsMissingBinary(t *testing.T) {
	// PATH without fastfetch must produce the typed error the handler maps to
	// a 503 rather than a generic failure.
	t.Setenv("PATH", t.TempDir())
	_, _, err := runFastfetch(context.Background(), nil, 1024)
	if !errors.Is(err, errUnavailable) {
		t.Fatalf("err = %v, want errUnavailable", err)
	}
}

func TestRunFastfetchProducesOutput(t *testing.T) {
	if _, err := exec.LookPath("fastfetch"); err != nil {
		t.Skip("fastfetch is not installed")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	output, truncated, err := runFastfetch(ctx, []string{"--structure", "OS"}, 64*1024)
	if err != nil {
		t.Fatalf("runFastfetch: %v", err)
	}
	if truncated {
		t.Fatalf("a single module truncated the buffer")
	}
	if !strings.Contains(output, "OS") {
		t.Fatalf("output missing the OS module: %q", output)
	}
	if strings.Contains(output, "\x1b") {
		t.Fatalf("output retained escape sequences: %q", output)
	}
}

func TestRunFastfetchSurfacesBadOptions(t *testing.T) {
	if _, err := exec.LookPath("fastfetch"); err != nil {
		t.Skip("fastfetch is not installed")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_, _, err := runFastfetch(ctx, []string{"--definitely-not-an-option"}, 64*1024)
	if err == nil {
		t.Fatal("runFastfetch accepted an unknown option")
	}
}
