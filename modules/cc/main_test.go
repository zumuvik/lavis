package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestReplaceHai(t *testing.T) {
	cases := map[string]string{
		"хай":               "йах",
		"ну хай хай":        "ну йах йах",
		"Хай":               "Йах",
		"ХАЙ":               "ЙАХ",
		"хайп":              "йахп",
		"ничего менять нет": "ничего менять нет",
	}
	for input, want := range cases {
		if got := replaceHai(input); got != want {
			t.Fatalf("replaceHai(%q) = %q, want %q", input, got, want)
		}
	}
}

func TestEventRequiresEnabledOutgoingMessage(t *testing.T) {
	mod := &module{state: state{Enabled: false}}
	payload := eventPayload{MessageRef: "opaque", Text: "хай", Outgoing: true}
	if got := mod.handleEvent("message.created", payload); len(got) != 0 {
		t.Fatalf("disabled module returned actions: %#v", got)
	}

	mod.state.Enabled = true
	payload.Outgoing = false
	if got := mod.handleEvent("message.created", payload); len(got) != 0 {
		t.Fatalf("incoming message returned actions: %#v", got)
	}
}

func TestEventEditsMatchingOutgoingText(t *testing.T) {
	mod := &module{state: state{Enabled: true}}
	got := mod.handleEvent("message.created", eventPayload{
		MessageRef: "opaque",
		Text:       "ну хай",
		Outgoing:   true,
	})
	if len(got) != 1 {
		t.Fatalf("got %d actions, want 1", len(got))
	}
	if got[0].Type != "message.edit" || got[0].MessageRef != "opaque" || got[0].Text != "ну йах" {
		t.Fatalf("unexpected action: %#v", got[0])
	}
}

func TestEventIgnoresCommaPrefixedText(t *testing.T) {
	mod := &module{state: state{Enabled: true}}
	got := mod.handleEvent("message.created", eventPayload{
		MessageRef: "opaque",
		Text:       ",say хай",
		Outgoing:   true,
	})
	if len(got) != 0 {
		t.Fatalf("comma-prefixed message returned actions: %#v", got)
	}
}

func TestCommandsPersistState(t *testing.T) {
	dir := t.TempDir()
	mod := &module{path: filepath.Join(dir, "state.json")}

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
