package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestReverseText(t *testing.T) {
	cases := map[string]string{
		"привет":      "тевирп",
		"ну привет":   "тевирп ун",
		"hello world": "dlrow olleh",
		"Привет 👋":   "👋 тевирП",
		"":            "",
	}
	for input, want := range cases {
		if got := reverseText(input); got != want {
			t.Fatalf("reverseText(%q) = %q, want %q", input, got, want)
		}
	}
}

func TestEventRequiresEnabledOutgoingMessage(t *testing.T) {
	mod := &module{state: state{Enabled: false}, expected: make(map[string]string)}
	payload := eventPayload{MessageRef: "opaque", MessageKey: "key", Text: "привет", Outgoing: true}
	if got := mod.handleEvent("message.created", payload); len(got) != 0 {
		t.Fatalf("disabled module returned actions: %#v", got)
	}

	mod.state.Enabled = true
	payload.Outgoing = false
	if got := mod.handleEvent("message.created", payload); len(got) != 0 {
		t.Fatalf("incoming message returned actions: %#v", got)
	}
}

func TestEventReversesOutgoingText(t *testing.T) {
	mod := &module{state: state{Enabled: true}, expected: make(map[string]string)}
	got := mod.handleEvent("message.created", eventPayload{
		MessageRef: "opaque",
		MessageKey: "key",
		Text:       "ну привет",
		Outgoing:   true,
	})
	if len(got) != 1 {
		t.Fatalf("got %d actions, want 1", len(got))
	}
	if got[0].Type != "message.edit" || got[0].MessageRef != "opaque" || got[0].Text != "тевирп ун" {
		t.Fatalf("unexpected action: %#v", got[0])
	}
}

func TestSelfEditDoesNotReverseBack(t *testing.T) {
	mod := &module{state: state{Enabled: true}, expected: make(map[string]string)}
	created := eventPayload{MessageRef: "opaque", MessageKey: "key", Text: "привет", Outgoing: true}
	got := mod.handleEvent("message.created", created)
	if len(got) != 1 || got[0].Text != "тевирп" {
		t.Fatalf("unexpected create action: %#v", got)
	}

	edited := eventPayload{MessageRef: "opaque-2", MessageKey: "key", Text: "тевирп", Outgoing: true}
	if got := mod.handleEvent("message.edited", edited); len(got) != 0 {
		t.Fatalf("self edit reversed back: %#v", got)
	}
}

func TestManualEditIsReversedAgain(t *testing.T) {
	mod := &module{state: state{Enabled: true}, expected: make(map[string]string)}
	created := eventPayload{MessageRef: "opaque", MessageKey: "key", Text: "привет", Outgoing: true}
	_ = mod.handleEvent("message.created", created)
	_ = mod.handleEvent("message.edited", eventPayload{MessageRef: "opaque-2", MessageKey: "key", Text: "тевирп", Outgoing: true})

	got := mod.handleEvent("message.edited", eventPayload{MessageRef: "opaque-3", MessageKey: "key", Text: "пока", Outgoing: true})
	if len(got) != 1 || got[0].Text != "акоп" {
		t.Fatalf("manual edit was not reversed: %#v", got)
	}
}

func TestEventIgnoresCommaPrefixedText(t *testing.T) {
	mod := &module{state: state{Enabled: true}, expected: make(map[string]string)}
	got := mod.handleEvent("message.created", eventPayload{
		MessageRef: "opaque",
		MessageKey: "key",
		Text:       ",say привет",
		Outgoing:   true,
	})
	if len(got) != 0 {
		t.Fatalf("comma-prefixed message returned actions: %#v", got)
	}
}

func TestCommandsPersistState(t *testing.T) {
	dir := t.TempDir()
	mod := &module{path: filepath.Join(dir, "state.json"), expected: make(map[string]string)}

	if text, err := mod.execute("e", ""); err != nil || text != "CC: включён" {
		t.Fatalf("enable = %q, %v", text, err)
	}
	data, err := os.ReadFile(mod.path)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "{\n  \"enabled\": true\n}\n" {
		t.Fatalf("unexpected enabled state: %q", data)
	}

	if text, err := mod.execute("d", ""); err != nil || text != "CC: выключен" {
		t.Fatalf("disable = %q, %v", text, err)
	}
	data, err = os.ReadFile(mod.path)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "{\n  \"enabled\": false\n}\n" {
		t.Fatalf("unexpected disabled state: %q", data)
	}
}
