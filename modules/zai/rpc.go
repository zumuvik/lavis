package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sync"
	"time"
)

const (
	protocolVersion = 6
	maxLineBytes    = 64 * 1024
)

var rpcTimeout = 10 * time.Second

// rpcTransport implements the Module API v6 host.invoke transport. Frames are
// written synchronously to out (a shared lineWriter); responses are routed by
// the stdin loop through the pending map keyed by call_id. The stdin loop
// never runs request handlers, so a waiting invoke can always receive its
// host.result.
type rpcTransport struct {
	mu      sync.Mutex
	nextID  int
	pending map[string]chan *hostResult
	out     io.Writer
}

type hostResult struct {
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
	return &rpcTransport{nextID: 1, pending: make(map[string]chan *hostResult), out: out}
}

type resultFrame struct {
	Type   string          `json:"type"`
	CallID string          `json:"call_id"`
	Ok     bool            `json:"ok"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *rpcError       `json:"error,omitempty"`
}

// dispatchAsync routes a host.result frame to its waiting rpc. Returns true
// when the line was a result frame, matched or not: an unmatched id means the
// waiter already timed out, and feeding such a frame back as a request would
// emit an UNKNOWN_TYPE error the host cannot correlate to any request.
func (t *rpcTransport) dispatchAsync(line []byte) bool {
	var frame resultFrame
	if err := json.Unmarshal(line, &frame); err != nil {
		return false
	}
	if frame.Type != "host.result" {
		return false
	}
	t.mu.Lock()
	ch, ok := t.pending[frame.CallID]
	if ok {
		delete(t.pending, frame.CallID)
	}
	t.mu.Unlock()
	if ok {
		ch <- &hostResult{Ok: frame.Ok, Result: frame.Result, Error: frame.Error}
		close(ch)
	}
	return true
}

// invokeHost sends a host.invoke frame and waits for its host.result.
func (t *rpcTransport) invokeHost(ctx context.Context, method string, params map[string]any) (*hostResult, error) {
	t.mu.Lock()
	id := fmt.Sprintf("zai-%d-%d", os.Getpid(), t.nextID)
	t.nextID++
	ch := make(chan *hostResult, 1)
	t.pending[id] = ch
	t.mu.Unlock()

	frame := map[string]any{
		"protocol_version": protocolVersion,
		"type":             "host.invoke",
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

// hostCaller sends host.invoke requests and requires an ok result. Host error
// messages are static sanitized strings supplied by Lavis.
type hostCaller struct{ rpc *rpcTransport }

func (c *hostCaller) hostCall(ctx context.Context, method string, params map[string]any) error {
	result, err := c.rpc.invokeHost(ctx, method, params)
	if err != nil {
		return err
	}
	if !result.Ok {
		message := ""
		if result.Error != nil {
			message = result.Error.Message
		}
		if message != "" {
			return fmt.Errorf("host %s: %s", method, message)
		}
		return fmt.Errorf("host %s failed", method)
	}
	return nil
}

// lineWriter serializes every outbound frame. Response and host.invoke frames
// share one stdout pipe, and a frame larger than PIPE_BUF is not written
// atomically; a torn newline corrupts both sides.
type lineWriter struct {
	mu sync.Mutex
	w  io.Writer
}

// WriteLine emits one complete JSON line under the lock. Write exists so the
// writer satisfies io.Writer, but callers must pass a whole line.
func (l *lineWriter) Write(data []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.w.Write(data)
}

func (l *lineWriter) WriteLine(data []byte) error {
	l.mu.Lock()
	defer l.mu.Unlock()
	_, err := l.w.Write(append(data, '\n'))
	return err
}

func encodeFrame(value any) ([]byte, error) {
	var buf bytes.Buffer
	encoder := json.NewEncoder(&buf)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimRight(buf.Bytes(), "\n"), nil
}
