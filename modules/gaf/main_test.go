package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf16"
)

func TestContainsWordPrefix(t *testing.T) {
	for _, text := range []string{"фур", "фури", "фурре", "ФУРРИ", "про фурре!"} {
		if !containsWord(text, "фур") {
			t.Fatalf("word prefix should match %q", text)
		}
	}
	if containsWord("антифур", "фур") {
		t.Fatal("trigger must start at a word boundary")
	}
}

func TestParseThreeReactionsIncludingPremium(t *testing.T) {
	values, err := parseReactions(
		[]token{{Text: "👍", StartUTF16: 5, EndUTF16: 7}, {Text: "x", StartUTF16: 8, EndUTF16: 9}, {Text: "❤️", StartUTF16: 10, EndUTF16: 12}},
		[]customEmojiEntity{{Type: "custom_emoji", OffsetUTF16: 8, LengthUTF16: 1, DocumentID: "5456140674028019486"}},
	)
	if err != nil {
		t.Fatal(err)
	}
	if len(values) != 3 {
		t.Fatalf("got %d reactions", len(values))
	}
	if values[1].Type != "custom_emoji" {
		t.Fatalf("middle reaction: %#v", values[1])
	}
}

func testModule(t *testing.T) module {
	t.Helper()
	return module{
		path: filepath.Join(t.TempDir(), "state.json"),
		state: state{
			Enabled: true,
			NextID:  2,
			Triggers: []trigger{{
				ID: 1, Word: "лайк", Enabled: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
			}},
			Active: map[string]activeEntry{},
		},
	}
}

func TestReactionsForMatchesTriggerPrefix(t *testing.T) {
	m := module{
		state: state{
			Enabled: true,
			Triggers: []trigger{{
				ID: 1, Word: "фур", Enabled: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "🐈"}},
			}},
		},
	}
	for _, text := range []string{"фури", "фурре"} {
		reactions := m.reactionsFor(text)
		if len(reactions) != 1 || reactions[0].Emoji != "🐈" {
			t.Fatalf("unexpected reactions for %q: %#v", text, reactions)
		}
	}
	if reactions := m.reactionsFor("антифур"); len(reactions) != 0 {
		t.Fatalf("embedded trigger must not match: %#v", reactions)
	}
}

func TestEditedMessageRemovesOwnedReaction(t *testing.T) {
	m := testModule(t)
	created := m.handleEvent("message.created", eventPayload{EventID: "10", MessageRef: "one", MessageKey: "stable", Text: "лайк"})
	if len(created) != 1 || len(created[0].Reactions) != 1 {
		t.Fatalf("created: %#v", created)
	}
	edited := m.handleEvent("message.edited", eventPayload{EventID: "11", MessageRef: "two", MessageKey: "stable", Text: "без слова"})
	if len(edited) != 1 || edited[0].Reactions == nil || len(edited[0].Reactions) != 0 {
		t.Fatalf("edited: %#v", edited)
	}
}

func TestEditedEventWinsOverLateCreatedEvent(t *testing.T) {
	m := testModule(t)
	removed := m.handleEvent("message.edited", eventPayload{EventID: "11", MessageRef: "edited", MessageKey: "same", Text: "нет"})
	if len(removed) != 0 {
		t.Fatalf("unexpected removal for an unknown message: %#v", removed)
	}
	late := m.handleEvent("message.created", eventPayload{EventID: "10", MessageRef: "created", MessageKey: "same", Text: "лайк"})
	if len(late) != 0 {
		t.Fatalf("late created event must be ignored: %#v", late)
	}
}

func TestDefaultGAFSubcommandShiftsPremiumEntityOffsets(t *testing.T) {
	m := module{
		path:  filepath.Join(t.TempDir(), "state.json"),
		state: state{Enabled: true, NextID: 1, Active: map[string]activeEntry{}},
	}
	arguments := "setr никс | x"
	entityOffset := len(utf16.Encode([]rune("setr никс | ")))
	_, err := m.execute("gaf", arguments, []customEmojiEntity{{
		Type: "custom_emoji", OffsetUTF16: entityOffset, LengthUTF16: 1, DocumentID: "5456140674028019486",
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(m.state.Triggers) != 1 || m.state.Triggers[0].Reactions[0].Type != "custom_emoji" {
		t.Fatalf("trigger: %#v", m.state.Triggers)
	}
}

func TestLoadModuleUsesDedicatedModuleStateDirectory(t *testing.T) {
	stateDir := t.TempDir()
	t.Setenv("LAVIS_MODULE_STATE_DIR", stateDir)
	t.Setenv("XDG_STATE_HOME", filepath.Join(t.TempDir(), "xdg-state"))

	initial := []byte(`{"enabled":true,"next_id":2,"triggers":[{"id":1,"word":"никс","reactions":[{"type":"emoji","emoji":"👍"}],"enabled":true}],"active":{}}`)
	if err := os.WriteFile(filepath.Join(stateDir, "state.json"), initial, 0o600); err != nil {
		t.Fatal(err)
	}

	m, err := loadModule()
	if err != nil {
		t.Fatal(err)
	}
	if m.path != filepath.Join(stateDir, "state.json") {
		t.Fatalf("unexpected state path: %s", m.path)
	}
	if len(m.state.Triggers) != 1 || m.state.Triggers[0].Word != "никс" {
		t.Fatalf("unexpected restored state: %#v", m.state)
	}
	if _, err := m.set("лавис | ❤️", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(stateDir, "state.json")); err != nil {
		t.Fatal(err)
	}
}

func TestSetUsesPipeRuleAndSupportsMultiwordTrigger(t *testing.T) {
	m := module{
		path:  filepath.Join(t.TempDir(), "state.json"),
		state: state{Enabled: true, NextID: 1, Active: map[string]activeEntry{}},
	}
	text, err := m.set("очень никс | 👍 ❤️", nil)
	if err != nil {
		t.Fatal(err)
	}
	if text != "✅ очень никс | 👍 ❤️" {
		t.Fatalf("unexpected response: %q", text)
	}
	if len(m.state.Triggers) != 1 || m.state.Triggers[0].Word != "очень никс" {
		t.Fatalf("trigger: %#v", m.state.Triggers)
	}
}

func TestSetRejectsMissingPipe(t *testing.T) {
	m := module{
		path:  filepath.Join(t.TempDir(), "state.json"),
		state: state{Enabled: true, NextID: 1, Active: map[string]activeEntry{}},
	}
	if _, err := m.set("никс 👍", nil); err == nil {
		t.Fatal("missing pipe must be rejected")
	}
}

func TestToggleSupportsMultiwordTrigger(t *testing.T) {
	m := module{
		path: filepath.Join(t.TempDir(), "state.json"),
		state: state{
			Enabled: true,
			NextID:  2,
			Triggers: []trigger{{
				ID: 1, Word: "очень никс", Enabled: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
			}},
			Active: map[string]activeEntry{},
		},
	}
	if _, err := m.toggle("очень никс off"); err != nil {
		t.Fatal(err)
	}
	if m.state.Triggers[0].Enabled {
		t.Fatal("multiword trigger should be disabled")
	}
}

func TestToggleSupportsTriggerEndingInSwitchWord(t *testing.T) {
	m := module{
		path: filepath.Join(t.TempDir(), "state.json"),
		state: state{
			Enabled: true,
			NextID:  2,
			Triggers: []trigger{{
				ID: 1, Word: "turn off", Enabled: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
			}},
			Active: map[string]activeEntry{},
		},
	}
	if _, err := m.toggle("turn off"); err != nil {
		t.Fatal(err)
	}
	if m.state.Triggers[0].Enabled {
		t.Fatal("exact trigger name must win over switch parsing")
	}
}

func TestRuntimeRevisionIsNotPersisted(t *testing.T) {
	m := testModule(t)
	m.state.Active["stable"] = activeEntry{
		Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
		Revision:  999,
		SeenAt:    1,
	}
	data, err := json.Marshal(m.state)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(data), `"revision"`) {
		t.Fatalf("runtime revision leaked into state: %s", data)
	}
	var restored state
	if err := json.Unmarshal(data, &restored); err != nil {
		t.Fatal(err)
	}
	restarted := module{path: filepath.Join(t.TempDir(), "state.json"), state: restored}
	actions := restarted.handleEvent("message.edited", eventPayload{
		EventID: "1", MessageRef: "edited", MessageKey: "stable", Text: "без триггера",
	})
	if len(actions) != 1 || actions[0].Reactions == nil || len(actions[0].Reactions) != 0 {
		t.Fatalf("restart must accept the new edit and remove reactions: %#v", actions)
	}
}

func TestCreatedNonMatchDoesNotPopulateActiveState(t *testing.T) {
	m := testModule(t)
	actions := m.handleEvent("message.created", eventPayload{
		EventID: "10", MessageRef: "created", MessageKey: "no-match", Text: "обычное сообщение",
	})
	if len(actions) != 0 || len(m.state.Active) != 0 {
		t.Fatalf("nonmatching created event should be ignored: actions=%#v active=%#v", actions, m.state.Active)
	}
}

func TestEventResultSerializesNoOpActionsAsEmptyArray(t *testing.T) {
	m := testModule(t)
	response := m.handle(request{
		ProtocolVersion: protocolVersion,
		Type:            "event",
		RequestID:       "no-op",
		Event:           "message.created",
		Payload: eventPayload{
			EventID: "10", MessageRef: "message", MessageKey: "no-match", Text: "обычное сообщение",
		},
	})
	data, err := json.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	var wire struct {
		Type    string          `json:"type"`
		Actions json.RawMessage `json:"actions"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		t.Fatal(err)
	}
	if wire.Type != "event_result" || string(wire.Actions) != "[]" {
		t.Fatalf("unexpected event result: %s", data)
	}
}

func TestEventResultSerializesReactionActions(t *testing.T) {
	m := testModule(t)
	response := m.handle(request{
		ProtocolVersion: protocolVersion,
		Type:            "event",
		RequestID:       "match",
		Event:           "message.created",
		Payload: eventPayload{
			EventID: "10", MessageRef: "message", MessageKey: "match", Text: "лайк",
		},
	})
	data, err := json.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	var wire struct {
		Actions []eventAction `json:"actions"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		t.Fatal(err)
	}
	if len(wire.Actions) != 1 || wire.Actions[0].Type != "message.react" || wire.Actions[0].MessageRef != "message" || len(wire.Actions[0].Reactions) != 1 || wire.Actions[0].Reactions[0] != (reaction{Type: "emoji", Emoji: "👍"}) {
		t.Fatalf("unexpected reaction actions: %s", data)
	}
}

func TestMalformedCustomIDIsRejected(t *testing.T) {
	if _, err := parseReactions([]token{{Text: "ce:not-a-number"}}, nil); err == nil {
		t.Fatal("malformed diagnostic custom emoji ID must be rejected")
	}
}
