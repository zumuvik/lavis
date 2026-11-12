package main

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/gotd/td/bin"
	"github.com/gotd/td/tg"
)

func TestParseCommand(t *testing.T) {
	for _, tc := range []struct {
		input          string
		chat           int64
		action, target string
		wantErr        bool
	}{
		{"on", 42, "on", "42", false},
		{"on @alice", 0, "on", "@alice", false},
		{"target -100", 0, "target", "-100", false},
		{"off", 0, "off", "", false},
		{"off @alice", 0, "off", "@alice", false},
		{"list", 0, "list", "", false},
		{"list extra", 0, "", "", true},
		{"on", 0, "on", "", false},
		{"target bad!", 0, "", "", true},
	} {
		a, b, e := parseCommand(tc.input, tc.chat)
		if (e != nil) != tc.wantErr || a != tc.action || b != tc.target {
			t.Errorf("parseCommand(%q)=(%q,%q,%v)", tc.input, a, b, e)
		}
	}
}

func TestResolveUsernameAndNumericDialog(t *testing.T) {
	user := &tg.User{ID: 123}
	user.SetAccessHash(456)
	encode := func(value bin.Encoder) []byte {
		t.Helper()
		var buf bin.Buffer
		if err := value.Encode(&buf); err != nil {
			t.Fatal(err)
		}
		return buf.Copy()
	}
	resolved := encode(&tg.ContactsResolvedPeer{Peer: &tg.PeerUser{UserID: 123}, Users: []tg.UserClass{user}})
	dialogs := encode(&tg.MessagesDialogs{Dialogs: []tg.DialogClass{&tg.Dialog{Peer: &tg.PeerUser{UserID: 123}}}, Users: []tg.UserClass{user}})
	m := &module{raw: func(_ context.Context, req bin.Encoder) ([]byte, error) {
		switch req.(type) {
		case *tg.ContactsResolveUsernameRequest:
			return resolved, nil
		case *tg.MessagesGetDialogsRequest:
			return dialogs, nil
		default:
			t.Fatalf("unexpected %T", req)
			return nil, nil
		}
	}}
	for _, target := range []string{"@alice", "123"} {
		peer, err := m.resolve(context.Background(), target)
		if err != nil {
			t.Fatal(err)
		}
		got, ok := peer.(*tg.InputPeerUser)
		if !ok || got.UserID != 123 || got.AccessHash != 456 {
			t.Fatalf("%s: %#v", target, peer)
		}
	}
}

func TestInputPeerNeedsAccessHash(t *testing.T) {
	user := &tg.User{ID: 123}
	if got := inputFromPeer(&tg.PeerUser{UserID: 123}, []tg.UserClass{user}, nil); got != nil {
		t.Fatalf("accepted user without access hash: %#v", got)
	}
	if got := inputFromPeer(&tg.PeerChat{ChatID: 42}, nil, nil); got == nil {
		t.Fatal("basic chat does not require access hash")
	}
}

func TestActivateStateTransitions(t *testing.T) {
	m := &module{target: "@saved", call: func(context.Context, string, bool) error { return errors.New("host failed") }}
	if err := m.activate(context.Background(), "@next"); err == nil {
		t.Fatal("expected invoke failure")
	}
	if len(m.active) != 0 || m.target != "@saved" {
		t.Fatalf("failed activation changed state: %+v", m)
	}
	var calls []string
	m.call = func(_ context.Context, target string, typing bool) error {
		calls = append(calls, target+":"+map[bool]string{true: "on", false: "off"}[typing])
		return nil
	}
	if err := m.activate(context.Background(), m.target); err != nil {
		t.Fatal(err)
	}
	if len(m.active) != 1 || m.target != "@saved" {
		t.Fatalf("not active on stored target: %+v", m)
	}
	if err := m.stopWithContext(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(m.active) != 0 || len(calls) != 2 || calls[0] != "@saved:on" || calls[1] != "@saved:off" {
		t.Fatalf("unexpected transitions: active=%v calls=%v", m.active, calls)
	}
}

func TestListAndIndependentTargets(t *testing.T) {
	var calls []string
	m := &module{call: func(_ context.Context, target string, typing bool) error {
		if typing {
			calls = append(calls, "+"+target)
		} else {
			calls = append(calls, "-"+target)
		}
		return nil
	}}
	for _, target := range []string{"@zoe", "123"} {
		if err := m.activate(context.Background(), target); err != nil {
			t.Fatal(err)
		}
	}
	if err := m.activate(context.Background(), "123"); err != nil {
		t.Fatal(err)
	}
	if len(calls) != 2 {
		t.Fatalf("duplicate target invoked twice: %v", calls)
	}
	now := time.Now()
	entry := m.active["123"]
	entry.started = now.Add(-2*time.Minute - 5*time.Second)
	m.active["123"] = entry
	got := m.list(now)
	if !strings.HasPrefix(got, "Печатает для:\n123 — 00:02:05\n@zoe — ") {
		t.Fatalf("wrong list or order: %q", got)
	}
	if err := m.stopTarget(context.Background(), "123"); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(m.list(now), "123") || len(m.active) != 1 {
		t.Fatal("stopping one target stopped another")
	}
	if err := m.stopWithContext(context.Background()); err != nil {
		t.Fatal(err)
	}
	if m.list(now) != "Активных целей нет" || len(calls) != 4 || calls[2] != "-123" || calls[3] != "-@zoe" {
		t.Fatalf("unexpected stop or empty list: %v %q", calls, m.list(now))
	}
}

func TestActiveTargetLimit(t *testing.T) {
	m := &module{call: func(context.Context, string, bool) error { return nil }}
	for id := 1; id <= maxActiveTargets; id++ {
		if err := m.activate(context.Background(), fmt.Sprint(id)); err != nil {
			t.Fatal(err)
		}
	}
	if err := m.activate(context.Background(), "extra"); err == nil {
		t.Fatal("limit ignored")
	}
	if len(m.active) != maxActiveTargets {
		t.Fatal("limit changed active targets")
	}
	if err := m.stopWithContext(context.Background()); err != nil {
		t.Fatal(err)
	}
}
