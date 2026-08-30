package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
)

type groupEntry struct {
	ID         int64  `json:"id"`
	AccessHash int64  `json:"access_hash,omitempty"`
	Title      string `json:"title"`
	Forum      bool   `json:"forum,omitempty"`
	// Left marks a channel the account has exited but whose dialog is
	// still present: exactly the ghost footprint opsec must surface.
	Left bool `json:"left,omitempty"`
}

type state struct {
	Enabled    bool         `json:"enabled"`
	Selected   []groupEntry `json:"selected"`
	Discovered []groupEntry `json:"discovered"`
	LastSync   int64        `json:"last_sync"`
	LastRun    int64        `json:"last_run"`

	// Frontier maps a selected group id to the unix time at which the
	// previous cleanup pass swept that group all the way to the bottom.
	// The next pass may then stop at messages older than frontier-12h
	// because everything older is already gone.
	Frontier map[int64]int64 `json:"frontier,omitempty"`

	Opsec          *opsecState `json:"opsec,omitempty"`
	LogChatID      int64       `json:"log_chat_id,omitempty"`
	LogAccessHash  int64       `json:"log_access_hash,omitempty"`
	LogTopicID     int         `json:"log_topic_id,omitempty"`
	LogTopicMarker string      `json:"log_topic_marker,omitempty"`
}

type moduleState struct {
	path  string
	state *state
}

type module struct {
	mu           sync.Mutex
	state        *state
	rpc          *rpcTransport
	call         *rawCaller
	out          *lineWriter
	path         string
	cleaning     atomic.Bool
	selfID       atomic.Int64
	opsecRunning atomic.Bool
	opsecPhase   atomic.Uint64
	selfHash     atomic.Int64
	purging      atomic.Bool
}

// beginPurge/endPurge serialize opsec purges against manual/scheduled
// cleanup passes and against each other.
func (m *module) beginPurge() bool { return m.purging.CompareAndSwap(false, true) }
func (m *module) endPurge()        { m.purging.Store(false) }

// beginClean/endClean serialize cleanup passes between the schedule ticker
// and manual runs; a pass is long and must never overlap itself.
func (m *module) beginClean() bool { return m.cleaning.CompareAndSwap(false, true) }
func (m *module) endClean()        { m.cleaning.Store(false) }

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
	out := &lineWriter{w: os.Stdout}
	rpc := newRPC(out)
	return &module{
		state: current,
		rpc:   rpc,
		call:  &rawCaller{rpc: rpc},
		out:   out,
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

// peekState runs fn with the state lock held for a pure read. fn must not
// perform RPC or block: handlers and the background loop share this lock.
func (m *module) peekState(fn func(*state)) {
	m.mu.Lock()
	defer m.mu.Unlock()
	fn(m.state)
}

// withState mutates the state and persists it atomically under the lock.
// fn must not perform RPC.
func (m *module) withState(fn func(*state) error) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if err := fn(m.state); err != nil {
		return err
	}
	return m.saveLocked()
}

// saveLocked persists the current state; callers hold m.mu.
func (m *module) saveLocked() error {
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

// logf posts a message to the Cleaner topic without blocking the caller.
func (m *module) logf(format string, args ...any) {
	m.logMessage(fmt.Sprintf(format, args...))
}

func (m *module) logMessage(text string) {
	var ref topicRef
	m.peekState(func(s *state) {
		ref = topicRef{chatID: s.LogChatID, accessHash: s.LogAccessHash, topicID: s.LogTopicID}
	})
	go func() {
		if err := m.postToTopic(context.Background(), ref, text); err != nil {
			fmt.Fprintln(os.Stderr, err)
		}
	}()
}
