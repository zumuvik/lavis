package main

import (
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf16"
)

func TestContainsWord(t *testing.T) {
	if !containsWord("поставь лайк!", "лайк") {
		t.Fatal("word should match")
	}
	if containsWord("лайкос", "лайк") {
		t.Fatal("substring must not match")
	}
	if !containsWord("ЛАЙК", "лайк") {
		t.Fatal("matching must ignore case")
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

func TestMalformedCustomIDIsRejected(t *testing.T) {
	if _, err := parseReactions([]token{{Text: "ce:not-a-number"}}, nil); err == nil {
		t.Fatal("malformed diagnostic custom emoji ID must be rejected")
	}
}
