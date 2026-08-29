package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sync"
	"time"

	"github.com/gotd/td/bin"
)

const (
	protocolVersion = 6
	maxLineBytes    = 64 * 1024
)

var rpcTimeout = 10 * time.Second

// rpcTransport implements the protocol_v6 parentless telegram.invoke
// transport. Frames are written synchronously to out (a shared lineWriter);
// responses are routed by the stdin loop through the pending map keyed by
// call_id. The stdin loop never runs request handlers, so a waiting invoke
// can always receive its telegram.result.
type rpcTransport struct {
	mu      sync.Mutex
	nextID  int
	pending map[string]chan *telegramResult
	out     io.Writer
}

type telegramResult struct {
	Ok     bool            `json:"ok"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *rpcError       `json:"error,omitempty"`
}

type rpcError struct {
	Kind              string `json:"kind"`
	Message           string `json:"message"`
	Code              *int   `json:"code"`
	Name              string `json:"name,omitempty"`
	RetryAfterSeconds *int   `json:"retry_after_seconds"`
}

func newRPC(out io.Writer) *rpcTransport {
	return &rpcTransport{nextID: 1, pending: make(map[string]chan *telegramResult), out: out}
}

type resultFrame struct {
	Type   string          `json:"type"`
	CallID string          `json:"call_id"`
	Ok     bool            `json:"ok"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *rpcError       `json:"error,omitempty"`
}

// dispatchAsync routes a telegram.result frame to its waiting rpc.
// Returns true when the line was a result frame, matched or not: an
// unmatched id means the waiter already timed out, and feeding such a frame
// back as a request would emit an UNKNOWN_TYPE error the host cannot
// correlate to any request.
func (t *rpcTransport) dispatchAsync(line []byte) bool {
	var frame resultFrame
	if err := json.Unmarshal(line, &frame); err != nil {
		return false
	}
	if frame.Type != "telegram.result" {
		return false
	}
	t.mu.Lock()
	ch, ok := t.pending[frame.CallID]
	if ok {
		delete(t.pending, frame.CallID)
	}
	t.mu.Unlock()
	if ok {
		ch <- &telegramResult{Ok: frame.Ok, Result: frame.Result, Error: frame.Error}
		close(ch)
	}
	return true
}

// invoke sends a raw.invoke frame and waits for its telegram.result.
func (t *rpcTransport) invoke(ctx context.Context, body []byte) (*telegramResult, error) {
	t.mu.Lock()
	id := fmt.Sprintf("cln-%d-%d", os.Getpid(), t.nextID)
	t.nextID++
	ch := make(chan *telegramResult, 1)
	t.pending[id] = ch
	t.mu.Unlock()

	frame := map[string]any{
		"protocol_version": 6,
		"type":             "telegram.invoke",
		"call_id":          id,
		"method":           "raw.invoke",
		"params": map[string]any{
			"body_base64_chunks": base64Chunks(body, 7168),
		},
	}
	line, err := encodeFrame(frame)
	if err != nil {
		t.mu.Lock()
		delete(t.pending, id)
		t.mu.Unlock()
		return nil, fmt.Errorf("marshal invoke: %w", err)
	}
	if err := t.writeFrame(line); err != nil {
		t.mu.Lock()
		delete(t.pending, id)
		t.mu.Unlock()
		return nil, fmt.Errorf("write invoke: %w", err)
	}

	timer := time.NewTimer(rpcTimeout)
	defer timer.Stop()
	select {
	case result := <-ch:
		if result == nil {
			return nil, fmt.Errorf("rpc closed without result")
		}
		return result, nil
	case <-ctx.Done():
		t.mu.Lock()
		delete(t.pending, id)
		t.mu.Unlock()
		return nil, fmt.Errorf("rpc cancelled: %w", ctx.Err())
	case <-timer.C:
		t.mu.Lock()
		delete(t.pending, id)
		t.mu.Unlock()
		return nil, fmt.Errorf("rpc timeout after %s", rpcTimeout)
	}
}

func (t *rpcTransport) writeFrame(line []byte) error {
	if writer, ok := t.out.(*lineWriter); ok {
		return writer.WriteLine(line)
	}
	_, err := t.out.Write(append(line, '\n'))
	return err
}

func base64Chunks(body []byte, chunk int) []string {
	if len(body) == 0 {
		return nil
	}
	encoded := base64.StdEncoding.EncodeToString(body)
	var chunks []string
	for len(encoded) > 0 {
		limit := chunk
		if len(encoded) < limit {
			limit = len(encoded)
		}
		chunks = append(chunks, encoded[:limit])
		encoded = encoded[limit:]
	}
	return chunks
}

// rawCaller wraps an rpcTransport with typed request/response helpers.
type rawCaller struct{ rpc *rpcTransport }

// call encodes the request, invokes raw.invoke and returns the opaque TL body.
func (c *rawCaller) call(ctx context.Context, request bin.Encoder) ([]byte, error) {
	var buf bin.Buffer
	if err := request.Encode(&buf); err != nil {
		return nil, fmt.Errorf("encode request: %w", err)
	}
	result, err := c.rpc.invoke(ctx, buf.Copy())
	if err != nil {
		return nil, err
	}
	if !result.Ok {
		if result.Error != nil && result.Error.Name != "" {
			return nil, fmt.Errorf("telegram %s", result.Error.Name)
		}
		return nil, fmt.Errorf("telegram rpc failed")
	}
	body, err := rawBody(result.Result)
	if err != nil {
		return nil, err
	}
	return normalizeConstructors(body), nil
}

type rawResultEnvelope struct {
	Kind   string   `json:"kind"`
	DcID   int      `json:"dc_id"`
	Chunks []string `json:"body_base64_chunks"`
}

func rawBody(raw json.RawMessage) ([]byte, error) {
	var envelope rawResultEnvelope
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return nil, fmt.Errorf("decode raw result: %w", err)
	}
	var joined string
	for _, chunk := range envelope.Chunks {
		joined += chunk
	}
	body, err := base64.StdEncoding.DecodeString(joined)
	if err != nil {
		return nil, fmt.Errorf("decode base64 body: %w", err)
	}
	if len(body)%4 != 0 {
		return nil, fmt.Errorf("raw body is not 4-byte aligned (%d)", len(body))
	}
	return body, nil
}

// gotd/td v0.161.0 is the newest release and predates the Telegram schema
// revision that rotated `channel` to id 0x1c32b11c. The generated field list
// is already identical to the current layer (only flags2.20 is unused extra),
// so responses are normalized by rewriting the rotated id to the pinned one.
// The 0x1c byte cannot legally appear in the string payloads that could
// otherwise false-match.
type constructorAlias struct {
	rotated []byte
	pinned  []byte
}

// Telegram rotates entity constructor ids across schema layers; gotd/td's
// newest release still decodes the previous ids with an otherwise compatible
// field layout. Only exact 4-byte constructor positions are rewritten; the
// rotated ids all contain bytes (0x1c, 0x88 control ranges) that cannot
// appear inside the string payloads of these responses.
var constructorAliases = []constructorAlias{
	{[]byte{0x1c, 0xb1, 0x32, 0x1c}, []byte{0xc6, 0x34, 0x9f, 0xd4}}, // 0x1c32b11c -> 0xd49f34c6 channel
	{[]byte{0x88, 0x43, 0x77, 0x31}, []byte{0x83, 0xcc, 0xb8, 0xb1}}, // 0x31774388 -> 0xb1b8cc83 user
}

func normalizeConstructors(body []byte) []byte {
	var out []byte
	for _, alias := range constructorAliases {
		if bytes.Index(body, alias.rotated) < 0 {
			continue
		}
		out = make([]byte, 0, len(body))
		for {
			i := bytes.Index(body, alias.rotated)
			if i < 0 {
				break
			}
			out = append(out, body[:i]...)
			out = append(out, alias.pinned...)
			body = body[i+len(alias.rotated):]
		}
		out = append(out, body...)
		body = out
	}
	if out == nil {
		return body
	}
	return out
}

func buffer(body []byte) *bin.Buffer {
	buf := new(bin.Buffer)
	buf.ResetTo(body)
	return buf
}
