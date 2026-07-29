package main

import (
	"path/filepath"
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
