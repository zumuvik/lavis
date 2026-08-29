package main

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"sync"
	"testing"
	"time"
)

func TestBase64ChunksRoundTrip(t *testing.T) {
	for _, size := range []int{0, 1, 4, 7167, 7168, 7169, 20000} {
		body := make([]byte, size)
		if size > 0 {
			rand.Read(body)
		}
		chunks := base64Chunks(body, 7168)
		var joined string
		for _, chunk := range chunks {
			if chunk == "" || len(chunk) > 7168 {
				t.Fatalf("bad chunk length %d", len(chunk))
			}
			joined += chunk
		}
		decoded, err := base64.StdEncoding.DecodeString(joined)
		if err != nil {
			t.Fatalf("decode: %v", err)
		}
		if !bytes.Equal(decoded, body) {
			t.Fatalf("round trip mismatch for size %d", size)
		}
	}
}

func TestInvokeRoundTrip(t *testing.T) {
	var mu sync.Mutex
	var written bytes.Buffer
	var rpc *rpcTransport
	rpc = newRPC(writerFunc(func(p []byte) (int, error) {
		// Parse the invoked frame, answer it like the Lavis host would.
		var line json.RawMessage = p
		// strip trailing newline
		line = bytes.TrimSuffix(line, []byte("\n"))
		var frame struct {
			Type   string `json:"type"`
			CallID string `json:"call_id"`
		}
		if err := json.Unmarshal(line, &frame); err != nil {
			t.Fatalf("unmarshal frame: %v", err)
		}
		if frame.Type != "telegram.invoke" {
			t.Fatalf("unexpected frame type: %s", frame.Type)
		}
		answer := `{"protocol_version":6,"type":"telegram.result","call_id":"` +
			frame.CallID + `","ok":true,"result":{"kind":"raw_tl","dc_id":1,"body_base64_chunks":["eFY0Eg=="]}}`
		rpc.dispatchAsync([]byte(answer))
		mu.Lock()
		defer mu.Unlock()
		return written.Write(p)
	}))

	result, err := rpc.invoke([]byte{0x11, 0x22, 0x33, 0x44})
	if err != nil {
		t.Fatalf("invoke: %v", err)
	}
	mu.Lock()
	body := written.String()
	mu.Unlock()
	if !result.Ok {
		t.Fatalf("expected ok result")
	}
	if !bytes.Contains([]byte(body), []byte(`"method":"raw.invoke"`)) {
		t.Fatalf("frame missing method: %s", body)
	}
}

func TestInvokeTimeout(t *testing.T) {
	rpc := newRPC(writerFunc(func(p []byte) (int, error) { return len(p), nil }))
	original := rpcTimeout
	defer func() { rpcTimeout = original }()
	rpcTimeout = 300 * time.Millisecond
	start := time.Now()
	_, err := rpc.invoke([]byte{0x11, 0x22, 0x33, 0x44})
	if err == nil {
		t.Fatal("expected timeout error")
	}
	if time.Since(start) > 2*time.Second {
		t.Fatal("timeout took too long")
	}
}

func TestRawBodyDecode(t *testing.T) {
	raw := json.RawMessage(`{"kind":"raw_tl","dc_id":1,"body_base64_chunks":["eFY0Eg=="]}`)
	body, err := rawBody(raw)
	if err != nil {
		t.Fatalf("rawBody: %v", err)
	}
	want := []byte{0x78, 0x56, 0x34, 0x12}
	if !bytes.Equal(body, want) {
		t.Fatalf("body mismatch: %x", body)
	}
}

func TestRawBodyRejectsMisaligned(t *testing.T) {
	raw := json.RawMessage(`{"kind":"raw_tl","dc_id":1,"body_base64_chunks":["c2hvcnQ="]}`)
	if _, err := rawBody(raw); err == nil {
		t.Fatal("expected alignment error")
	}
}

func TestStateRoundTrip(t *testing.T) {
	s := &state{
		Enabled:        true,
		Selected:       []groupEntry{{ID: 1, AccessHash: 2, Title: "A"}},
		Discovered:     []groupEntry{{ID: 3, AccessHash: 4, Title: "B"}},
		LastSync:       100,
		LogChatID:      5,
		LogTopicID:     6,
		LogTopicMarker: "Lavis",
	}
	data, err := json.Marshal(s)
	if err != nil {
		t.Fatal(err)
	}
	var back state
	if err := json.Unmarshal(data, &back); err != nil {
		t.Fatal(err)
	}
	if !back.Enabled || len(back.Selected) != 1 || back.LogTopicID != 6 {
		t.Fatalf("state mismatch: %+v", back)
	}
}

func TestScannerLineBoundary(t *testing.T) {
	output := bytes.Repeat([]byte{'x'}, 1<<20)
	scanner := bufio.NewScanner(bytes.NewReader(output))
	scanner.Buffer(make([]byte, 4096), maxLineBytes)
	_ = scanner
}

type writerFunc func(p []byte) (int, error)

func (f writerFunc) Write(p []byte) (int, error) { return f(p) }
