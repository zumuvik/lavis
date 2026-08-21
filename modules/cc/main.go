package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const protocolVersion = 6

type eventPayload struct {
	MessageRef string `json:"message_ref"`
	MessageKey string `json:"message_key"`
	Text       string `json:"text"`
	Outgoing   bool   `json:"outgoing"`
}

type request struct {
	ProtocolVersion int          `json:"protocol_version"`
	Type            string       `json:"type"`
	RequestID       string       `json:"request_id"`
	ModuleID        string       `json:"module_id"`
	Command         string       `json:"command"`
	Arguments       string       `json:"arguments"`
	Event           string       `json:"event"`
	Payload         eventPayload `json:"payload"`
}

type eventAction struct {
	Type       string `json:"type"`
	MessageRef string `json:"message_ref"`
	Text       string `json:"text"`
}

type response struct {
	ProtocolVersion int            `json:"protocol_version"`
	Type            string         `json:"type"`
	RequestID       string         `json:"request_id"`
	ModuleID        string         `json:"module_id,omitempty"`
	Text            string         `json:"text,omitempty"`
	Code            string         `json:"code,omitempty"`
	Message         string         `json:"message,omitempty"`
	Actions         *[]eventAction `json:"actions,omitempty"`
}

type state struct {
	Enabled bool `json:"enabled"`
}

type module struct {
	path     string
	state    state
	expected map[string]string
}

func main() {
	mod, err := loadModule()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), 64*1024)
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)

	for scanner.Scan() {
		var req request
		if err := json.Unmarshal(scanner.Bytes(), &req); err != nil {
			continue
		}
		resp, exit := mod.handle(req)
		if exit {
			return
		}
		if err := encoder.Encode(resp); err != nil {
			fmt.Fprintln(os.Stderr, err)
			return
		}
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func loadModule() (*module, error) {
	path, err := statePath()
	if err != nil {
		return nil, err
	}
	current := state{Enabled: false}
	data, err := os.ReadFile(path)
	if err == nil {
		if err := json.Unmarshal(data, &current); err != nil {
			return nil, fmt.Errorf("decode state: %w", err)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read state: %w", err)
	}
	return &module{path: path, state: current, expected: make(map[string]string)}, nil
}

func statePath() (string, error) {
	if dir := os.Getenv("LAVIS_MODULE_STATE_DIR"); dir != "" {
		return filepath.Join(dir, "state.json"), nil
	}
	if stateHome := os.Getenv("XDG_STATE_HOME"); stateHome != "" {
		return filepath.Join(stateHome, "lavis", "modules", "cc", "state.json"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("resolve home directory: %w", err)
	}
	return filepath.Join(home, ".local", "state", "lavis", "modules", "cc", "state.json"), nil
}

func (m *module) handle(req request) (response, bool) {
	base := response{ProtocolVersion: protocolVersion, RequestID: req.RequestID}
	if req.ProtocolVersion != protocolVersion {
		base.Type = "error"
		base.Code = "PROTOCOL_VERSION"
		base.Message = "unsupported protocol version"
		return base, false
	}

	switch req.Type {
	case "initialize":
		base.Type = "initialized"
		base.ModuleID = req.ModuleID
	case "health":
		base.Type = "health"
	case "shutdown":
		return response{}, true
	case "execute":
		base.Type = "result"
		text, err := m.execute(req.Command, req.Arguments)
		if err != nil {
			base.Type = "error"
			base.Code = "BAD_INPUT"
			base.Message = err.Error()
		} else {
			base.Text = text
		}
	case "event":
		base.Type = "event_result"
		actions := m.handleEvent(req.Event, req.Payload)
		base.Actions = &actions
	default:
		base.Type = "error"
		base.Code = "UNKNOWN_TYPE"
		base.Message = "unknown request type"
	}
	return base, false
}

func (m *module) execute(command, arguments string) (string, error) {
	if strings.TrimSpace(arguments) != "" {
		return "", errors.New("команда не принимает аргументы")
	}
	switch strings.ToLower(strings.TrimSpace(command)) {
	case "e":
		m.state.Enabled = true
		if err := m.save(); err != nil {
			return "", err
		}
		return "CC: включён", nil
	case "d":
		m.state.Enabled = false
		m.expected = make(map[string]string)
		if err := m.save(); err != nil {
			return "", err
		}
		return "CC: выключен", nil
	default:
		return "", fmt.Errorf("неизвестная команда: %s", command)
	}
}

func (m *module) handleEvent(event string, payload eventPayload) []eventAction {
	if !m.state.Enabled || !payload.Outgoing || payload.MessageRef == "" || payload.MessageKey == "" {
		return []eventAction{}
	}
	if event != "message.created" && event != "message.edited" {
		return []eventAction{}
	}
	if strings.HasPrefix(payload.Text, ",") {
		return []eventAction{}
	}
	if expected, ok := m.expected[payload.MessageKey]; ok && expected == payload.Text {
		delete(m.expected, payload.MessageKey)
		return []eventAction{}
	}

	rewritten := reverseText(payload.Text)
	if rewritten == payload.Text {
		return []eventAction{}
	}
	m.expected[payload.MessageKey] = rewritten
	return []eventAction{{
		Type:       "message.edit",
		MessageRef: payload.MessageRef,
		Text:       rewritten,
	}}
}

func reverseText(text string) string {
	runes := []rune(text)
	for left, right := 0, len(runes)-1; left < right; left, right = left+1, right-1 {
		runes[left], runes[right] = runes[right], runes[left]
	}
	return string(runes)
}

func (m *module) save() error {
	data, err := json.MarshalIndent(m.state, "", "  ")
	if err != nil {
		return fmt.Errorf("encode state: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(m.path), 0o700); err != nil {
		return fmt.Errorf("create state directory: %w", err)
	}
	temporary := m.path + ".tmp"
	if err := os.WriteFile(temporary, append(data, '\n'), 0o600); err != nil {
		return fmt.Errorf("write state: %w", err)
	}
	if err := os.Chmod(temporary, 0o600); err != nil {
		_ = os.Remove(temporary)
		return fmt.Errorf("secure state: %w", err)
	}
	if err := os.Rename(temporary, m.path); err != nil {
		_ = os.Remove(temporary)
		return fmt.Errorf("replace state: %w", err)
	}
	return nil
}
