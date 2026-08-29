package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
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

	result, err := rpc.invoke(context.Background(), []byte{0x11, 0x22, 0x33, 0x44})
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
	_, err := rpc.invoke(context.Background(), []byte{0x11, 0x22, 0x33, 0x44})
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

func TestDispatchConsumesStaleResults(t *testing.T) {
	rpc := newRPC(io.Discard)
	if !rpc.dispatchAsync([]byte(`{"protocol_version":6,"type":"telegram.result","call_id":"gone","ok":true}`)) {
		t.Fatal("stale telegram.result must be consumed, not republished as a request")
	}
	if rpc.dispatchAsync([]byte(`{"protocol_version":6,"type":"execute","request_id":"1"}`)) {
		t.Fatal("request lines must not be dispatched as results")
	}
}

func TestInvokeHonoursContextDeadline(t *testing.T) {
	rpc := newRPC(io.Discard)
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	start := time.Now()
	if _, err := rpc.invoke(ctx, []byte{0x11, 0x22, 0x33, 0x44}); err == nil {
		t.Fatal("expected cancellation error")
	}
	if time.Since(start) > 2*time.Second {
		t.Fatal("context deadline ignored")
	}
	rpc.mu.Lock()
	defer rpc.mu.Unlock()
	if len(rpc.pending) != 0 {
		t.Fatalf("cancelled invoke leaked %d pending calls", len(rpc.pending))
	}
}

func TestRemoveGroupsDropsEntry(t *testing.T) {
	m := &module{
		state: &state{
			Enabled:  true,
			Selected: []groupEntry{{ID: 1, Title: "A"}, {ID: 2, Title: "B"}, {ID: 3, Title: "C"}},
		},
		path: filepath.Join(t.TempDir(), "state.json"),
	}
	if _, err := m.removeGroups("2"); err != nil {
		t.Fatalf("removeGroups: %v", err)
	}
	got := m.state.Selected
	if len(got) != 2 || got[0].ID != 1 || got[1].ID != 3 {
		t.Fatalf("removed entry survived or wrong order: %+v", got)
	}
	data, err := os.ReadFile(m.path)
	if err != nil {
		t.Fatalf("state not persisted: %v", err)
	}
	var persisted state
	if err := json.Unmarshal(data, &persisted); err != nil {
		t.Fatal(err)
	}
	if len(persisted.Selected) != 2 {
		t.Fatalf("persisted state mismatch: %+v", persisted.Selected)
	}
}

func TestLineWriterSerializesFrames(t *testing.T) {
	var buf bytes.Buffer
	out := &lineWriter{w: writerFunc(func(p []byte) (int, error) { return buf.Write(p) })}
	var wg sync.WaitGroup
	for i := range 8 {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			data, err := encodeFrame(map[string]int{"n": i})
			if err != nil {
				t.Error(err)
				return
			}
			if err := out.WriteLine(data); err != nil {
				t.Error(err)
			}
		}(i)
	}
	wg.Wait()
	lines := bytes.Count(buf.Bytes(), []byte("\n"))
	if lines != 8 {
		t.Fatalf("expected 8 whole lines, got %d: %q", lines, buf.String())
	}
	scanner := bufio.NewScanner(bytes.NewReader(buf.Bytes()))
	for scanner.Scan() {
		var parsed map[string]int
		if err := json.Unmarshal(scanner.Bytes(), &parsed); err != nil {
			t.Fatalf("torn frame: %q: %v", scanner.Bytes(), err)
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
}
