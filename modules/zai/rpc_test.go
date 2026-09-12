package main

import (
	"bytes"
	"context"
	"encoding/json"
	"strings"
	"sync"
	"testing"
	"time"
)

// answeringWriter parses each written frame and answers host.invoke frames
// with the given ok flag (and errPayload when !ok), like the Lavis host.
type answeringWriter struct {
	t          *testing.T
	ok         bool
	errPayload string
	mu         sync.Mutex
	written    bytes.Buffer
	rpc        *rpcTransport
}

func (a *answeringWriter) Write(p []byte) (int, error) {
	line := bytes.TrimSuffix(append([]byte(nil), p...), []byte("\n"))
	var frame struct {
		Type   string `json:"type"`
		CallID string `json:"call_id"`
	}
	if err := json.Unmarshal(line, &frame); err != nil {
		a.t.Fatalf("unmarshal frame: %v", err)
	}
	if frame.Type == "host.invoke" {
		answer := `{"protocol_version":6,"type":"host.result","call_id":"` + frame.CallID + `","ok":` + boolJSON(a.ok)
		if !a.ok {
			answer += `,"error":{"kind":"host","message":"` + a.errPayload + `","code":null,"name":null,"retry_after_seconds":null}`
		}
		answer += `,"result":null}`
		a.rpc.dispatchAsync([]byte(answer))
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.written.Write(p)
}

func boolJSON(b bool) string {
	if b {
		return "true"
	}
	return "false"
}

func newTestTransport(t *testing.T, ok bool, errPayload string) (*answeringWriter, *hostCaller) {
	t.Helper()
	w := &answeringWriter{t: t, ok: ok, errPayload: errPayload}
	rpc := newRPC(w)
	w.rpc = rpc
	return w, &hostCaller{rpc: rpc}
}

// newTestModule is newTestTransport wired into a full module, as main() does.
func newTestModule(t *testing.T, ok bool, errPayload string) (*answeringWriter, *module) {
	t.Helper()
	w := &answeringWriter{t: t, ok: ok, errPayload: errPayload}
	out := &lineWriter{w: w}
	rpc := newRPC(out)
	w.rpc = rpc
	return w, &module{out: out, rpc: rpc, hc: &hostCaller{rpc: rpc}}
}

func TestHostInvokeRoundTrip(t *testing.T) {
	w, hc := newTestTransport(t, true, "")
	err := hc.hostCall(context.Background(), "inline.form", map[string]any{
		"peer": "peer-1",
		"text": "hello",
	})
	if err != nil {
		t.Fatalf("hostCall: %v", err)
	}
	body := w.written.String()
	for _, want := range []string{`"type":"host.invoke"`, `"method":"inline.form"`, `"peer":"peer-1"`, `"protocol_version":6`} {
		if !strings.Contains(body, want) {
			t.Fatalf("frame missing %q:\n%s", want, body)
		}
	}
	if !strings.Contains(body, `"call_id":"zai-`) {
		t.Fatalf("call_id prefix missing:\n%s", body)
	}
}

func TestHostInvokeSurfacesHostError(t *testing.T) {
	_, hc := newTestTransport(t, false, "bot send rejected")
	err := hc.hostCall(context.Background(), "inline.answer", map[string]any{
		"callback_id": "cb-1",
		"text":        "",
		"show_alert":  false,
	})
	if err == nil {
		t.Fatal("expected host error")
	}
	if !strings.Contains(err.Error(), "bot send rejected") {
		t.Fatalf("sanitized host message lost: %v", err)
	}
}

func TestBotCallbackEditsMenu(t *testing.T) {
	stubAPI(t, `{}`)
	writeTokenFile(t, "token=secret\n")
	w, m := newTestModule(t, true, "")

	resp := m.handle(request{ProtocolVersion: protocolVersion, RequestID: "ev-1", Type: "event", Event: "bot.callback", Payload: json.RawMessage(`{"callback_id":"cb-9","data":"usage","chat_id":-100123,"message_id":456,"from_user_id":789}`)})
	if resp.Type != "event_result" {
		t.Fatalf("event handling: %+v", resp)
	}
	if resp.Actions == nil || len(*resp.Actions) != 0 {
		t.Fatalf("expected empty actions: %+v", resp)
	}

	// The callback handler runs on a goroutine via handleEvent; wait for its
	// two host.invoke frames.
	waitFor(t, func() bool {
		w.mu.Lock()
		defer w.mu.Unlock()
		return strings.Count(w.written.String(), `"type":"host.invoke"`) == 2
	})
	body := w.written.String()
	for _, want := range []string{
		`"method":"inline.answer"`,
		`"callback_id":"cb-9"`,
		`"method":"message.editBot"`,
		`"chat_id":-100123`,
		`"message_id":456`,
		"Z.AI — использование моделей",
	} {
		if !strings.Contains(body, want) {
			t.Fatalf("callback frames missing %q:\n%s", want, body)
		}
	}
}

func TestBotCallbackInlineMessageIDUsesInlineEdit(t *testing.T) {
	stubAPI(t, `{}`)
	writeTokenFile(t, "token=secret\n")
	w, m := newTestModule(t, true, "")

	m.handle(request{ProtocolVersion: protocolVersion, RequestID: "ev-4", Type: "event", Event: "bot.callback", Payload: json.RawMessage(`{"callback_id":"cb-12","data":"quota","from_user_id":789,"inline_message_id":"AgAB-abc"}`)})
	waitFor(t, func() bool {
		w.mu.Lock()
		defer w.mu.Unlock()
		return strings.Count(w.written.String(), `"type":"host.invoke"`) == 2
	})
	body := w.written.String()
	if !strings.Contains(body, `"inline_message_id":"AgAB-abc"`) {
		t.Fatalf("inline_message_id lost:\n%s", body)
	}
	if strings.Contains(body, `"chat_id"`) || strings.Contains(body, `"message_id"`) {
		t.Fatalf("inline edit must not address the chat:\n%s", body)
	}
}

func TestBotCallbackCloseDeletes(t *testing.T) {
	stubAPI(t, `{}`)
	writeTokenFile(t, "token=secret\n")
	w, m := newTestModule(t, true, "")

	m.handle(request{ProtocolVersion: protocolVersion, RequestID: "ev-2", Type: "event", Event: "bot.callback", Payload: json.RawMessage(`{"callback_id":"cb-10","data":"close","chat_id":-100123,"message_id":456}`)})
	waitFor(t, func() bool {
		w.mu.Lock()
		defer w.mu.Unlock()
		return strings.Count(w.written.String(), `"method":"message.deleteBot"`) == 1
	})
	body := w.written.String()
	if !strings.Contains(body, `"chat_id":-100123`) || !strings.Contains(body, `"message_id":456`) {
		t.Fatalf("deleteBot missing target:\n%s", body)
	}
	if strings.Contains(body, `"method":"message.editBot"`) {
		t.Fatalf("close must not redraw the menu:\n%s", body)
	}
}

func TestBotCallbackUnknownData(t *testing.T) {
	w, m := newTestModule(t, true, "")

	m.handle(request{ProtocolVersion: protocolVersion, RequestID: "ev-3", Type: "event", Event: "bot.callback", Payload: json.RawMessage(`{"callback_id":"cb-11","data":"???"}`)})
	waitFor(t, func() bool {
		w.mu.Lock()
		defer w.mu.Unlock()
		return strings.Count(w.written.String(), `"type":"host.invoke"`) == 2
	})
	if !strings.Contains(w.written.String(), "❓ Неизвестное действие") {
		t.Fatalf("unknown-data menu redraw missing:\n%s", w.written.String())
	}
	if !strings.Contains(w.written.String(), `"message.editBot"`) {
		t.Fatalf("menu redraw must go through message.editBot:\n%s", w.written.String())
	}
}

// waitFor spins until cond holds; handleEvent answers callbacks
// asynchronously, so the frames are not written synchronously.
func waitFor(t *testing.T, cond func() bool) {
	t.Helper()
	for i := 0; i < 200; i++ {
		if cond() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("condition not reached")
}
