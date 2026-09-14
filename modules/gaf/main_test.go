package main

import (
	"context"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
	"unicode/utf16"
)

func mustPayload(t *testing.T, value any) json.RawMessage {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func TestContainsTriggerWordBoundary(t *testing.T) {
	for _, text := range []string{"лайк", "лайки", "лайкнул", "ЛАЙКНИ", "про лайкнул!"} {
		if !containsTrigger(text, "лайк", false, false) {
			t.Fatalf("word prefix should match %q", text)
		}
	}
	if containsTrigger("полайк", "лайк", false, false) {
		t.Fatal("trigger must start at a word boundary by default")
	}
}

func TestContainsTriggerBoundaryFlags(t *testing.T) {
	if containsTrigger("агаф", "гаф", false, false) {
		t.Fatal("mid-word match must require match_start (prefixes)")
	}
	if !containsTrigger("агаф", "гаф", true, false) {
		t.Fatal("match_start (prefixes) must fire mid-word")
	}
	if !containsTrigger("гафа", "гаф", false, false) {
		t.Fatal("trailing continuation stays allowed by default")
	}
	if containsTrigger("гафа", "гаф", false, true) {
		t.Fatal("match_end (whole word) must require a trailing word boundary")
	}
	if !containsTrigger("гаф", "гаф", false, true) {
		t.Fatal("whole word must match with match_end")
	}
	if containsTrigger("агафа", "гаф", true, true) {
		t.Fatal("embedded match must fail with both strict boundaries")
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
			StateVersion: stateVersion,
			Enabled:      true,
			NextID:       2,
			Triggers: []trigger{{
				ID: 1, Word: "лайк", Enabled: true, AllChats: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
			}},
			Active: map[string]activeEntry{},
			Scope: scopeConfig{
				DMs:           true,
				Groups:        true,
				ChatOverrides: map[int64]bool{},
			},
		},
	}
}

// testRPCModule builds a module whose host transport discards frames and
// times out fast: hostCall failures are the expected outcome without a host.
func testRPCModule(t *testing.T) *module {
	t.Helper()
	previous := rpcTimeout
	rpcTimeout = 50 * time.Millisecond
	t.Cleanup(func() { rpcTimeout = previous })
	out := &lineWriter{w: io.Discard}
	rpc := newRPC(out)
	m := &module{
		out: out, rpc: rpc, hc: &hostCaller{rpc: rpc},
		drafts: map[int]*draft{},
		path:   filepath.Join(t.TempDir(), "state.json"),
		state:  newState(),
	}
	return m
}

func TestReactionsForMatchesTriggerPrefix(t *testing.T) {
	m := module{
		state: state{
			Enabled: true,
			Triggers: []trigger{{
				ID: 1, Word: "лайк", Enabled: true, AllChats: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "🐈"}},
			}},
		},
	}
	for _, text := range []string{"лайки", "лайкнул"} {
		reactions, _ := m.reactionsFor(text, 42)
		if len(reactions) != 1 || reactions[0].Emoji != "🐈" {
			t.Fatalf("unexpected reactions for %q: %#v", text, reactions)
		}
	}
	if reactions, _ := m.reactionsFor("полайк", 42); len(reactions) != 0 {
		t.Fatalf("embedded trigger must not match: %#v", reactions)
	}
}

func TestReactionsForReturnsTriggerNote(t *testing.T) {
	m := module{
		state: state{
			Enabled: true,
			Triggers: []trigger{{
				ID: 1, Word: "лайк", Enabled: true, AllChats: true,
				Reactions: []reaction{{Type: "emoji", Emoji: "🐈"}},
			}},
		},
	}
	_, note := m.reactionsFor("лайки", 42)
	if note != "лайк" {
		t.Fatalf("note must name the fired trigger: %q", note)
	}
}

func TestReactionsForRespectsTriggerChatScope(t *testing.T) {
	m := module{
		state: state{
			Enabled: true,
			Triggers: []trigger{{
				ID: 1, Word: "лайк", Enabled: true, AllChats: false, ChatID: 100,
				Reactions: []reaction{{Type: "emoji", Emoji: "🐈"}},
			}},
		},
	}
	if reactions, _ := m.reactionsFor("лайки", 200); len(reactions) != 0 {
		t.Fatalf("scoped trigger fired in a foreign chat: %#v", reactions)
	}
	if reactions, _ := m.reactionsFor("лайки", 100); len(reactions) != 1 {
		t.Fatalf("scoped trigger must fire in its own chat: %#v", reactions)
	}
}

func TestScopeAllows(t *testing.T) {
	scope := scopeConfig{DMs: true, Groups: false, ChatOverrides: map[int64]bool{-1005: true, 7: false}}
	if !scopeAllows(scope, 42) {
		t.Fatal("DM must follow scope.dms")
	}
	if scopeAllows(scope, -1) {
		t.Fatal("group must follow scope.groups")
	}
	if !scopeAllows(scope, -1005) {
		t.Fatal("positive override must win over groups scope")
	}
	if scopeAllows(scope, 7) {
		t.Fatal("negative override must win over dms scope")
	}
}

func TestEditedMessageRemovesOwnedReaction(t *testing.T) {
	m := testModule(t)
	created := m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "10", MessageRef: "one", MessageKey: "stable", Text: "лайк",
	})})
	if len(created) != 1 || len(created[0].Reactions) != 1 {
		t.Fatalf("created: %#v", created)
	}
	edited := m.handleEvent(request{Type: "event", Event: "message.edited", Payload: mustPayload(t, eventPayload{
		EventID: "11", MessageRef: "two", MessageKey: "stable", Text: "без слова",
	})})
	if len(edited) != 1 || edited[0].Reactions == nil || len(edited[0].Reactions) != 0 {
		t.Fatalf("edited: %#v", edited)
	}
}

func TestEditedEventWinsOverLateCreatedEvent(t *testing.T) {
	m := testModule(t)
	removed := m.handleEvent(request{Type: "event", Event: "message.edited", Payload: mustPayload(t, eventPayload{
		EventID: "11", MessageRef: "edited", MessageKey: "same", Text: "нет",
	})})
	if len(removed) != 0 {
		t.Fatalf("unexpected removal for an unknown message: %#v", removed)
	}
	late := m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "10", MessageRef: "created", MessageKey: "same", Text: "лайк",
	})})
	if len(late) != 0 {
		t.Fatalf("late created event must be ignored: %#v", late)
	}
}

func TestEditorCommandRequiresHostMessageHandle(t *testing.T) {
	m := testRPCModule(t)
	if _, err := m.execute(context.Background(), "gaf", "setr никс | 👍", nil); err == nil {
		t.Fatal("editor must fail without a host message handle")
	}
}

func TestEditorCommandRejectsMissingPipe(t *testing.T) {
	m := testRPCModule(t)
	if _, err := m.execute(context.Background(), "gaf", "setr никс 👍", nil); err == nil {
		t.Fatal("missing pipe must be rejected")
	}
}

func TestSubcommandShiftsPremiumEntityOffsets(t *testing.T) {
	_, rest, offset := splitFirstWithOffset("setr никс | x")
	if rest != "никс | x" {
		t.Fatalf("unexpected rest: %q", rest)
	}
	_, reactionText, reactionOffset, err := splitTriggerRule(rest)
	if err != nil {
		t.Fatal(err)
	}
	entityOffset := len(utf16.Encode([]rune("setr никс | ")))
	shifted := shiftEntities([]customEmojiEntity{{
		Type: "custom_emoji", OffsetUTF16: entityOffset, LengthUTF16: 1, DocumentID: "5456140674028019486",
	}}, offset+reactionOffset)
	values, err := parseReactions(tokenize(reactionText), shifted)
	if err != nil {
		t.Fatal(err)
	}
	if len(values) != 1 || values[0].Type != "custom_emoji" {
		t.Fatalf("shifted premium entity must parse: %#v", values)
	}
}

func TestLoadStateMigratesV1AndUsesModuleStateDirectory(t *testing.T) {
	stateDir := t.TempDir()
	t.Setenv("LAVIS_MODULE_STATE_DIR", stateDir)
	t.Setenv("XDG_STATE_HOME", filepath.Join(t.TempDir(), "xdg-state"))

	initial := []byte(`{"enabled":true,"next_id":2,"triggers":[{"id":1,"word":"никс","reactions":[{"type":"emoji","emoji":"👍"}],"enabled":true}],"active":{}}`)
	if err := os.WriteFile(filepath.Join(stateDir, "state.json"), initial, 0o600); err != nil {
		t.Fatal(err)
	}

	path, err := statePath()
	if err != nil {
		t.Fatal(err)
	}
	if path != filepath.Join(stateDir, "state.json") {
		t.Fatalf("unexpected state path: %s", path)
	}
	loaded, err := loadState(path)
	if err != nil {
		t.Fatal(err)
	}
	if loaded.StateVersion != stateVersion {
		t.Fatalf("state must migrate to v2: %#v", loaded)
	}
	if !loaded.Scope.DMs || !loaded.Scope.Groups {
		t.Fatalf("v1 scope must default to everywhere: %#v", loaded.Scope)
	}
	if len(loaded.Triggers) != 1 || loaded.Triggers[0].Word != "никс" || !loaded.Triggers[0].AllChats {
		t.Fatalf("v1 triggers must keep everywhere behavior: %#v", loaded.Triggers)
	}

	m := newModule(path, loaded)
	if err := m.save(); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(stateDir, "state.json")); err != nil {
		t.Fatal(err)
	}
}

func TestToggleSupportsMultiwordTrigger(t *testing.T) {
	m := testModule(t)
	m.state.Triggers[0].Word = "очень никс"
	if _, err := m.toggle("очень никс off"); err != nil {
		t.Fatal(err)
	}
	if m.state.Triggers[0].Enabled {
		t.Fatal("multiword trigger should be disabled")
	}
}

func TestToggleSupportsTriggerEndingInSwitchWord(t *testing.T) {
	m := testModule(t)
	m.state.Triggers[0].Word = "turn off"
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
	actions := restarted.handleEvent(request{Type: "event", Event: "message.edited", Payload: mustPayload(t, eventPayload{
		EventID: "1", MessageRef: "edited", MessageKey: "stable", Text: "без триггера",
	})})
	if len(actions) != 1 || actions[0].Reactions == nil || len(actions[0].Reactions) != 0 {
		t.Fatalf("restart must accept the new edit and remove reactions: %#v", actions)
	}
}

func TestCreatedNonMatchDoesNotPopulateActiveState(t *testing.T) {
	m := testModule(t)
	actions := m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "10", MessageRef: "created", MessageKey: "no-match", Text: "обычное сообщение",
	})})
	if len(actions) != 0 || len(m.state.Active) != 0 {
		t.Fatalf("nonmatching created event should be ignored: actions=%#v active=%#v", actions, m.state.Active)
	}
}

func TestLifecycleOutsideScopeIsIgnored(t *testing.T) {
	m := testModule(t)
	m.state.Scope.Groups = false
	actions := m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "10", MessageRef: "group", MessageKey: "g1", PeerID: -100123, Text: "лайк",
	})})
	if len(actions) != 0 {
		t.Fatalf("reactions outside scope must not fire: %#v", actions)
	}
}

func TestReactionActionCarriesTriggerNote(t *testing.T) {
	m := testModule(t)
	response := m.handle(request{
		ProtocolVersion: protocolVersion,
		Type:            "event",
		RequestID:       "match",
		Event:           "message.created",
		Payload: mustPayload(t, eventPayload{
			EventID: "10", MessageRef: "message", MessageKey: "match", Text: "лайк",
		}),
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
	if wire.Actions[0].Note != "лайк" {
		t.Fatalf("action must carry the trigger note: %#v", wire.Actions[0])
	}
}

func TestEventResultSerializesNoOpActionsAsEmptyArray(t *testing.T) {
	m := testModule(t)
	response := m.handle(request{
		ProtocolVersion: protocolVersion,
		Type:            "event",
		RequestID:       "no-op",
		Event:           "message.created",
		Payload: mustPayload(t, eventPayload{
			EventID: "10", MessageRef: "message", MessageKey: "no-match", Text: "обычное сообщение",
		}),
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

func TestMalformedCustomIDIsRejected(t *testing.T) {
	if _, err := parseReactions([]token{{Text: "ce:not-a-number"}}, nil); err == nil {
		t.Fatal("malformed diagnostic custom emoji ID must be rejected")
	}
}

func TestWaitingCaptureRewritesDraftAndDeletesInput(t *testing.T) {
	m := testRPCModule(t)
	d := &draft{Word: "старое", ChatID: 100}
	id := m.storeDraft(d)
	m.waiting = &waiting{ChatID: 100, DraftID: id, ArmedAt: time.Now().Add(-time.Second)}

	actions := m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "10", MessageRef: "input", MessageKey: "w1", PeerID: 100, Outgoing: true, Text: "новое | 👍",
	})})
	if len(actions) != 0 {
		t.Fatalf("captured input must not react: %#v", actions)
	}
	if m.waiting != nil {
		t.Fatal("waiting must be cleared after capture")
	}
	if d.Word != "новое" || len(d.Reactions) != 1 || d.Reactions[0].Emoji != "👍" {
		t.Fatalf("draft must adopt the captured rule: %#v", d)
	}
}

func TestWaitingCaptureIgnoresForeignAndIncomingMessages(t *testing.T) {
	m := testRPCModule(t)
	d := &draft{Word: "старое", ChatID: 100}
	id := m.storeDraft(d)
	m.waiting = &waiting{ChatID: 100, DraftID: id, ArmedAt: time.Now().Add(-time.Second)}

	m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "11", MessageRef: "foreign", MessageKey: "w2", PeerID: 200, Outgoing: true, Text: "новое | 👍",
	})})
	if d.Word != "старое" || m.waiting == nil {
		t.Fatal("a foreign chat must not be captured")
	}

	m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "12", MessageRef: "incoming", MessageKey: "w3", PeerID: 100, Text: "новое | 👍",
	})})
	if d.Word != "старое" || m.waiting == nil {
		t.Fatal("incoming messages must not be captured")
	}
}

func TestWaitingCaptureExpires(t *testing.T) {
	m := testRPCModule(t)
	d := &draft{Word: "старое", ChatID: 100}
	id := m.storeDraft(d)
	m.waiting = &waiting{ChatID: 100, DraftID: id, ArmedAt: time.Now().Add(-2 * waitingTTL)}

	m.handleEvent(request{Type: "event", Event: "message.created", Payload: mustPayload(t, eventPayload{
		EventID: "13", MessageRef: "late", MessageKey: "w4", PeerID: 100, Outgoing: true, Text: "новое | 👍",
	})})
	if d.Word != "старое" {
		t.Fatal("expired waiting must not capture")
	}
	if m.waiting != nil {
		t.Fatal("expired waiting must be cleared")
	}
}

func TestEditorCallbackTogglesScopeRadio(t *testing.T) {
	m := testRPCModule(t)
	d := &draft{Word: "гаф", AllChats: true, ChatID: 100}
	id := m.storeDraft(d)
	m.chatID = 100

	m.handleEvent(request{Type: "event", Event: "bot.callback", Payload: mustPayload(t, botCallback{
		CallbackID: "cb1", Data: "o" + strconv.Itoa(id), InlineMessageID: "im1",
	})})
	if d.AllChats {
		t.Fatal("Только этот чат must clear all_chats")
	}
	m.handleEvent(request{Type: "event", Event: "bot.callback", Payload: mustPayload(t, botCallback{
		CallbackID: "cb2", Data: "a" + strconv.Itoa(id), InlineMessageID: "im1",
	})})
	if !d.AllChats {
		t.Fatal("Все чаты must set all_chats")
	}
}

func TestMainMenuCallbackTogglesGroupsScope(t *testing.T) {
	m := testRPCModule(t)
	m.chatID = 42

	m.handleEvent(request{Type: "event", Event: "bot.callback", Payload: mustPayload(t, botCallback{
		CallbackID: "cb1", Data: "g", InlineMessageID: "im1",
	})})
	if m.state.Scope.Groups {
		t.Fatal("groups toggle must flip scope.groups")
	}
	_ = context.Background()
}
