package main

import (
	"context"
	"errors"
	"testing"

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
	if m.active || m.cancel != nil || m.target != "@saved" {
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
	if !m.active || m.target != "@saved" {
		t.Fatalf("not active on stored target: %+v", m)
	}
	if err := m.stopWithContext(context.Background()); err != nil {
		t.Fatal(err)
	}
	if m.active || len(calls) != 2 || calls[0] != "@saved:on" || calls[1] != "@saved:off" {
		t.Fatalf("unexpected transitions: active=%v calls=%v", m.active, calls)
	}
}
