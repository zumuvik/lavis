package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

type groupEntry struct {
	ID         int64  `json:"id"`
	AccessHash int64  `json:"access_hash,omitempty"`
	Title      string `json:"title"`
	Forum      bool   `json:"forum,omitempty"`
}

type state struct {
	Enabled        bool         `json:"enabled"`
	Selected       []groupEntry `json:"selected"`
	Discovered     []groupEntry `json:"discovered"`
	LastSync       int64        `json:"last_sync"`
	LastRun        int64        `json:"last_run"`
	LogChatID      int64        `json:"log_chat_id,omitempty"`
	LogAccessHash  int64        `json:"log_access_hash,omitempty"`
	LogTopicID     int          `json:"log_topic_id,omitempty"`
	LogTopicMarker string       `json:"log_topic_marker,omitempty"`
}

type moduleState struct {
	path  string
	state *state
}

type module struct {
	state *state
	rpc   *rpcTransport
	call  *rawCaller
	path  string
}

func loadModule() (*module, error) {
	path, err := statePath()
	if err != nil {
		return nil, err
	}
	current := &state{Enabled: true}
	data, err := os.ReadFile(path)
	if err == nil {
		if err := json.Unmarshal(data, current); err != nil {
			return nil, fmt.Errorf("decode state: %w", err)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read state: %w", err)
	}
	if current.Selected == nil {
		current.Selected = []groupEntry{}
	}
	if current.Discovered == nil {
		current.Discovered = []groupEntry{}
	}
	rpc := newRPC(os.Stdout)
	return &module{
		state: current,
		rpc:   rpc,
		call:  &rawCaller{rpc: rpc},
		path:  path,
	}, nil
}

func statePath() (string, error) {
	if moduleStateDir := os.Getenv("LAVIS_MODULE_STATE_DIR"); moduleStateDir != "" {
		return filepath.Join(moduleStateDir, "state.json"), nil
	}
	if stateHome := os.Getenv("XDG_STATE_HOME"); stateHome != "" {
		return filepath.Join(stateHome, "lavis", "modules", "cleaner", "state.json"), nil
	}
	executable, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("resolve executable: %w", err)
	}
	return filepath.Join(filepath.Dir(executable), "state.json"), nil
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

// logf posts a message to the Cleaner topic asynchronously; the placeholder
// ack channel is used only to make the provider function convenient.
func (m *module) logf(format string, args ...any) {
	go func() {
		if err := m.sendLogMessage(fmt.Sprintf(format, args...)); err != nil {
			fmt.Fprintln(os.Stderr, err)
		}
	}()
}

func (m *module) logMessage(text string) {
	go func() {
		if err := m.sendLogMessage(text); err != nil {
			fmt.Fprintln(os.Stderr, err)
		}
	}()
}
