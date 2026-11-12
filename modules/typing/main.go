package main

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/gotd/td/bin"
	"github.com/gotd/td/tg"
)

const protocolVersion = 6

type request struct {
	ProtocolVersion int    `json:"protocol_version"`
	Type            string `json:"type"`
	RequestID       string `json:"request_id"`
	ModuleID        string `json:"module_id"`
	Command         string `json:"command"`
	Arguments       string `json:"arguments"`
	Context         *struct {
		ChatID int64 `json:"chat_id"`
	} `json:"context,omitempty"`
}
type response struct {
	ProtocolVersion int     `json:"protocol_version"`
	Type            string  `json:"type"`
	RequestID       string  `json:"request_id"`
	ModuleID        string  `json:"module_id,omitempty"`
	Text            *string `json:"text,omitempty"`
	Code            string  `json:"code,omitempty"`
	Message         string  `json:"message,omitempty"`
}
type invoke struct {
	ProtocolVersion int            `json:"protocol_version"`
	Type            string         `json:"type"`
	CallID          string         `json:"call_id"`
	Method          string         `json:"method"`
	Params          map[string]any `json:"params"`
}
type result struct {
	Type   string          `json:"type"`
	CallID string          `json:"call_id"`
	Ok     bool            `json:"ok"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *struct {
		Message string `json:"message"`
	} `json:"error,omitempty"`
}

type module struct {
	out      *bufio.Writer
	active   bool
	target   string
	cancel   context.CancelFunc
	loopDone chan struct{}
	next     int
	mu       sync.Mutex
	pending  map[string]chan result
	call     func(context.Context, string, bool) error
	raw      func(context.Context, bin.Encoder) ([]byte, error)
	peers    map[string]tg.InputPeerClass
}

func main() {
	m := &module{out: bufio.NewWriter(os.Stdout), pending: make(map[string]chan result)}
	lines := make(chan []byte, 8)
	done := make(chan struct{})
	go func() {
		defer close(done)
		for line := range lines {
			var r request
			if json.Unmarshal(line, &r) == nil {
				m.handle(r)
			}
		}
	}()
	s := bufio.NewScanner(os.Stdin)
	s.Buffer(make([]byte, 4096), 64*1024)
	for s.Scan() {
		b := append([]byte(nil), s.Bytes()...)
		var x result
		if json.Unmarshal(b, &x) == nil && x.Type == "telegram.result" {
			m.mu.Lock()
			ch := m.pending[x.CallID]
			delete(m.pending, x.CallID)
			m.mu.Unlock()
			if ch != nil {
				ch <- x
			}
			continue
		}
		lines <- b
	}
	close(lines)
	<-done
	m.stop()
}

func parseCommand(args string, chatID int64) (string, string, error) {
	f := strings.Fields(args)
	if len(f) == 0 {
		return "", "", errors.New("usage: typing on [target] | off | target <target>")
	}
	switch strings.ToLower(f[0]) {
	case "on":
		if len(f) > 2 {
			return "", "", errors.New("usage: typing on [target]")
		}
		target := ""
		if len(f) == 2 {
			target = f[1]
		} else if chatID != 0 {
			target = strconv.FormatInt(chatID, 10)
		} else {
			return "on", "", nil
		}
		if target != "" && !validTarget(target) {
			return "", "", errors.New("target must be @username or numeric ID")
		}
		return "on", target, nil
	case "off":
		if len(f) != 1 {
			return "", "", errors.New("usage: typing off")
		}
		return "off", "", nil
	case "target":
		if len(f) != 2 || !validTarget(f[1]) {
			return "", "", errors.New("usage: typing target @username|numeric-ID")
		}
		return "target", f[1], nil
	default:
		return "", "", errors.New("usage: typing on [target] | off | target <target>")
	}
}
func validTarget(s string) bool {
	if strings.HasPrefix(s, "@") {
		username := s[1:]
		if len(username) == 0 || len(username) > 32 {
			return false
		}
		for _, c := range username {
			if !((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_') {
				return false
			}
		}
		return true
	}
	_, err := parseDialogID(s)
	return err == nil
}

func (m *module) handle(r request) {
	b := response{ProtocolVersion: protocolVersion, RequestID: r.RequestID}
	if r.ProtocolVersion != protocolVersion {
		b.Type = "error"
		b.Code = "PROTOCOL_VERSION"
		b.Message = "unsupported protocol version"
		m.write(b)
		return
	}
	switch r.Type {
	case "initialize":
		b.Type = "initialized"
		b.ModuleID = r.ModuleID
	case "health":
		b.Type = "health"
	case "shutdown":
		m.stop()
		os.Exit(0)
	case "execute":
		b.Type = "result"
		chat := int64(0)
		if r.Context != nil {
			chat = r.Context.ChatID
		}
		command := strings.TrimSpace(r.Command)
		args := r.Arguments
		if command == "typing" {
			command = ""
		}
		if command != "" {
			args = command + " " + args
		}
		action, target, err := parseCommand(args, chat)
		if err == nil {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			switch action {
			case "off":
				err = m.stopWithContext(ctx)
			case "target":
				if m.active {
					err = m.activate(ctx, target)
				} else {
					m.target = target
				}
			case "on":
				if len(strings.Fields(args)) == 1 && m.target != "" {
					target = m.target
				}
				if target == "" {
					err = errors.New("укажите цель: typing target @username|ID")
				} else {
					err = m.activate(ctx, target)
				}
			}
		}
		if err != nil {
			b.Type = "error"
			b.Code = "BAD_INPUT"
			b.Message = "Ошибка: " + err.Error()
		} else {
			text := "Индикатор включён"
			if action == "off" {
				text = "Индикатор выключен"
			}
			if action == "target" {
				text = "Цель установлена: " + target
			}
			b.Text = &text
		}
	default:
		b.Type = "error"
		b.Code = "UNKNOWN_TYPE"
		b.Message = "unknown request type"
	}
	m.write(b)
}
func (m *module) activate(ctx context.Context, target string) error {
	if m.active && m.target == target {
		return nil
	}
	if m.active {
		if err := m.stopWithContext(ctx); err != nil {
			return err
		}
	}
	if err := m.callTyping(ctx, target, true); err != nil {
		return err
	}
	run, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	m.cancel = cancel
	m.loopDone = done
	m.target = target
	m.active = true
	go func() { defer close(done); m.loop(run, target) }()
	return nil
}
func (m *module) callTyping(ctx context.Context, target string, typing bool) error {
	if m.call != nil {
		return m.call(ctx, target, typing)
	}
	return m.invoke(ctx, target, typing)
}
func (m *module) stop() {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	_ = m.stopWithContext(ctx)
}
func (m *module) stopWithContext(ctx context.Context) error {
	if !m.active {
		return nil
	}
	m.active = false
	if m.cancel != nil {
		m.cancel()
		m.cancel = nil
	}
	if m.loopDone != nil {
		select {
		case <-m.loopDone:
		case <-ctx.Done():
			return ctx.Err()
		}
		m.loopDone = nil
	}
	return m.callTyping(ctx, m.target, false)
}
func (m *module) loop(ctx context.Context, target string) {
	ticker := time.NewTicker(4 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			c, cancel := context.WithTimeout(ctx, 5*time.Second)
			err := m.callTyping(c, target, true)
			cancel()
			if err != nil {
				fmt.Fprintln(os.Stderr, "typing refresh:", err)
			}
		}
	}
}
func (m *module) invoke(ctx context.Context, target string, typing bool) error {
	peer := m.peers[target]
	if peer == nil {
		var err error
		peer, err = m.resolve(ctx, target)
		if err != nil {
			return err
		}
		if m.peers == nil {
			m.peers = make(map[string]tg.InputPeerClass)
		}
		m.peers[target] = peer
	}
	action := tg.SendMessageActionClass(&tg.SendMessageTypingAction{})
	if !typing {
		action = &tg.SendMessageCancelAction{}
	}
	body, err := m.rawCall(ctx, &tg.MessagesSetTypingRequest{Peer: peer, Action: action})
	if err != nil {
		delete(m.peers, target)
		return err
	}
	status, err := tg.DecodeBool(rawBuffer(body))
	if err != nil {
		return fmt.Errorf("decode typing response: %w", err)
	}
	if _, ok := status.(*tg.BoolTrue); !ok {
		return errors.New("typing rejected by Telegram")
	}
	return nil
}

func (m *module) rawInvoke(ctx context.Context, body []byte) ([]byte, error) {
	m.mu.Lock()
	m.next++
	id := fmt.Sprintf("typing-%d", m.next)
	ch := make(chan result, 1)
	m.pending[id] = ch
	m.mu.Unlock()
	data, _ := json.Marshal(invoke{protocolVersion, "telegram.invoke", id, "raw.invoke", map[string]any{"body_base64_chunks": []string{base64.StdEncoding.EncodeToString(body)}}})
	if err := m.writeRaw(data); err != nil {
		m.mu.Lock()
		delete(m.pending, id)
		m.mu.Unlock()
		return nil, err
	}
	select {
	case r := <-ch:
		if !r.Ok {
			if r.Error != nil {
				return nil, errors.New(r.Error.Message)
			}
			return nil, errors.New("telegram invoke failed")
		}
		var envelope struct {
			Kind   string   `json:"kind"`
			Chunks []string `json:"body_base64_chunks"`
		}
		if err := json.Unmarshal(r.Result, &envelope); err != nil {
			return nil, err
		}
		if envelope.Kind != "raw_tl" || len(envelope.Chunks) == 0 {
			return nil, errors.New("invalid raw result")
		}
		decoded, err := base64.StdEncoding.DecodeString(strings.Join(envelope.Chunks, ""))
		if err != nil {
			return nil, err
		}
		return normalizeConstructors(decoded), nil
	case <-ctx.Done():
		m.mu.Lock()
		delete(m.pending, id)
		m.mu.Unlock()
		return nil, ctx.Err()
	}
}
func (m *module) write(v any) {
	b, e := json.Marshal(v)
	if e == nil {
		_ = m.writeRaw(b)
	}
}
func (m *module) writeRaw(b []byte) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	_, e := m.out.Write(append(b, '\n'))
	if e != nil {
		return e
	}
	return m.out.Flush()
}
