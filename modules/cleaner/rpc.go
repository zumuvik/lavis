package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
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

// TelegramRPCError keeps the host-reported RPC failure name and the
// FLOOD_WAIT hint so callers can sleep and retry instead of aborting.
type TelegramRPCError struct {
	Name       string
	Kind       string
	Message    string
	RetryAfter time.Duration
}

func (e *TelegramRPCError) Error() string {
	if e.Name != "" {
		return "telegram " + e.Name
	}
	if e.Message != "" {
		return fmt.Sprintf("telegram rpc failure kind=%s message=%s", e.Kind, e.Message)
	}
	return "telegram rpc failure kind=" + e.Kind
}

// asFloodWait returns how long to wait when err is a FLOOD_WAIT the caller
// can act on, bounded so a pass never parks indefinitely.
func asFloodWait(err error) (time.Duration, bool) {
	var rpcErr *TelegramRPCError
	if !errors.As(err, &rpcErr) || rpcErr.Name != "FLOOD_WAIT" {
		return 0, false
	}
	if rpcErr.RetryAfter <= 0 || rpcErr.RetryAfter > 5*time.Minute {
		return 0, false
	}
	return rpcErr.RetryAfter, true
}

// callWithFloodRetry runs an RPC, transparently honouring FLOOD_WAIT hints.
func callWithFloodRetry(ctx context.Context, op func() error) error {
	for attempt := 0; ; attempt++ {
		err := op()
		if err == nil {
			return nil
		}
		wait, flood := asFloodWait(err)
		if !flood || attempt >= 10 {
			return err
		}
		timer := time.NewTimer(wait + time.Second)
		select {
		case <-timer.C:
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		}
		timer.Stop()
	}
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

// dispatchAsync routes a telegram.result or host.result frame to its waiting
// rpc. Returns true when the line was a result frame, matched or not: an
// unmatched id means the waiter already timed out, and feeding such a frame
// back as a request would emit an UNKNOWN_TYPE error the host cannot
// correlate to any request.
func (t *rpcTransport) dispatchAsync(line []byte) bool {
	var frame resultFrame
	if err := json.Unmarshal(line, &frame); err != nil {
		return false
	}
	if frame.Type != "telegram.result" && frame.Type != "host.result" {
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
	return t.invokeFrame(ctx, "telegram.invoke", "raw.invoke", map[string]any{
		"body_base64_chunks": base64Chunks(body, 7168),
	})
}

// invokeHost sends a host.invoke frame and waits for its host.result.
func (t *rpcTransport) invokeHost(ctx context.Context, method string, params map[string]any) (*telegramResult, error) {
	return t.invokeFrame(ctx, "host.invoke", method, params)
}

func (t *rpcTransport) invokeFrame(ctx context.Context, frameType, method string, params map[string]any) (*telegramResult, error) {
	t.mu.Lock()
	id := fmt.Sprintf("cln-%d-%d", os.Getpid(), t.nextID)
	t.nextID++
	ch := make(chan *telegramResult, 1)
	t.pending[id] = ch
	t.mu.Unlock()

	frame := map[string]any{
		"protocol_version": 6,
		"type":             frameType,
		"call_id":          id,
		"method":           method,
		"params":           params,
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
		failure := &TelegramRPCError{}
		if result.Error != nil {
			failure.Name = result.Error.Name
			failure.Kind = result.Error.Kind
			failure.Message = result.Error.Message
			if result.Error.RetryAfterSeconds != nil && *result.Error.RetryAfterSeconds > 0 {
				failure.RetryAfter = time.Duration(*result.Error.RetryAfterSeconds) * time.Second
			}
		}
		if failure.Name == "" && failure.Kind == "" {
			failure.Kind = "rpc_failed"
		}
		return nil, failure
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

// hostCall sends a host.invoke request and requires an ok result. Host error
// messages are static sanitized strings supplied by Lavis.
func (c *rawCaller) hostCall(ctx context.Context, method string, params map[string]any) error {
	result, err := c.rpc.invokeHost(ctx, method, params)
	if err != nil {
		return err
	}
	if !result.Ok {
		failure := &TelegramRPCError{Kind: "host"}
		if result.Error != nil {
			failure.Kind = result.Error.Kind
			failure.Message = result.Error.Message
		}
		if failure.Message != "" {
			failure.Kind = "host"
			return fmt.Errorf("host %s: %s", method, failure.Message)
		}
		return fmt.Errorf("host %s failed", method)
	}
	return nil
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
