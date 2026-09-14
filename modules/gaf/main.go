// Command gaf is a Lavis external module (Module API v6, contract revision 5)
// that reacts to trigger words with up to three emoji/premium reactions.
// It answers the "gaf" command with an inline companion-bot menu, persists
// its triggers, and consumes message.created/edited events plus bot.callback
// presses for menu interaction.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf16"
	"unicode/utf8"
)

const (
	protocolVersion = 6
	// commandBudget must terminate before the host's 5s lifecycle deadline.
	commandBudget     = 4 * time.Second
	maxReactions      = 3
	maxTriggers       = 128
	maxActiveMessages = 4096
	maxTriggerRunes   = 64
	maxNoteRunes      = 128
	maxListUTF16      = 4096
	stateVersion      = 2
	waitingTTL        = 120 * time.Second
)

type customEmojiEntity struct {
	Type        string `json:"type"`
	OffsetUTF16 int    `json:"offset_utf16"`
	LengthUTF16 int    `json:"length_utf16"`
	DocumentID  string `json:"document_id"`
}

type requestContext struct {
	Peer              string              `json:"peer,omitempty"`
	Message           string              `json:"message,omitempty"`
	Text              string              `json:"text,omitempty"`
	ChatID            int64               `json:"chat_id,omitempty"`
	ArgumentEntities  []customEmojiEntity `json:"argument_entities"`
}

type eventPayload struct {
	EventID    string              `json:"event_id"`
	MessageRef string              `json:"message_ref"`
	MessageKey string              `json:"message_key"`
	PeerID     int64               `json:"peer_id,omitempty"`
	Text       string              `json:"text"`
	Outgoing   bool                `json:"outgoing"`
	Entities   []customEmojiEntity `json:"entities"`
}

type botCallback struct {
	CallbackID      string `json:"callback_id"`
	Data            string `json:"data"`
	ChatID          int64  `json:"chat_id"`
	MessageID       int64  `json:"message_id"`
	FromUserID      int64  `json:"from_user_id"`
	InlineMessageID string `json:"inline_message_id"`
}

type request struct {
	ProtocolVersion int             `json:"protocol_version"`
	Type            string          `json:"type"`
	RequestID       string          `json:"request_id"`
	ModuleID        string          `json:"module_id"`
	Command         string          `json:"command"`
	Arguments       string          `json:"arguments"`
	Event           string          `json:"event"`
	Payload         json.RawMessage `json:"payload,omitempty"`
	Context         *requestContext `json:"context,omitempty"`
}

type reaction struct {
	Type       string `json:"type"`
	Emoji      string `json:"emoji,omitempty"`
	DocumentID string `json:"document_id,omitempty"`
}

type eventAction struct {
	Type       string     `json:"type"`
	MessageRef string     `json:"message_ref"`
	Reactions  []reaction `json:"reactions"`
	Note       string     `json:"note,omitempty"`
}

type response struct {
	ProtocolVersion int            `json:"protocol_version"`
	Type            string         `json:"type"`
	RequestID       string         `json:"request_id"`
	ModuleID        string         `json:"module_id,omitempty"`
	Text            *string        `json:"text,omitempty"`
	Code            string         `json:"code,omitempty"`
	Message         string         `json:"message,omitempty"`
	Actions         *[]eventAction `json:"actions,omitempty"`
}

type inlineButton struct {
	Text string `json:"text"`
	Data string `json:"data"`
}

type trigger struct {
	ID         int        `json:"id"`
	Word       string     `json:"word"`
	Reactions  []reaction `json:"reactions"`
	Enabled    bool       `json:"enabled"`
	MatchStart bool       `json:"match_start"`
	MatchEnd   bool       `json:"match_end"`
	AllChats   bool       `json:"all_chats"`
	// ChatID scopes the trigger to one chat when AllChats is false
	// (Bot API dialog id format: positive users, negative chats/channels).
	ChatID int64 `json:"chat_id,omitempty"`
}

type scopeConfig struct {
	DMs           bool           `json:"dms"`
	Groups        bool           `json:"groups"`
	ChatOverrides map[int64]bool `json:"chat_overrides"`
}

type activeEntry struct {
	Reactions []reaction `json:"reactions"`
	Revision  uint64     `json:"-"`
	SeenAt    int64      `json:"seen_at"`
}

type state struct {
	StateVersion int                    `json:"state_version"`
	Enabled      bool                   `json:"enabled"`
	NextID       int                    `json:"next_id"`
	Triggers     []trigger              `json:"triggers"`
	Active       map[string]activeEntry `json:"active"`
	Scope        scopeConfig            `json:"scope"`
}

// waiting is the armed "edit by example" capture: the module absorbs the next
// outgoing message.created in the same chat as `<word> | <reactions>`. It is
// strictly in-memory and expires; it is never persisted.
type waiting struct {
	ChatID          int64
	DraftID         int
	InlineMessageID string
	CallbackChatID  int64
	CallbackMsgID   int64
	ArmedAt         time.Time
}

// draft is the in-memory editor state behind an open trigger editor menu.
// Callback buttons reference it by id because a full trigger does not fit
// the 32-byte callback-data budget.
type draft struct {
	Word       string
	Reactions  []reaction
	MatchStart bool
	MatchEnd   bool
	AllChats   bool
	Exists     bool
	TriggerID  int
	ChatID     int64
}

type token struct {
	Text       string
	StartUTF16 int
	EndUTF16   int
}

type module struct {
	path string
	out  *lineWriter
	rpc  *rpcTransport
	hc   *hostCaller

	context *requestContext
	chatID  int64

	state     state
	waiting   *waiting
	drafts    map[int]*draft
	nextDraft int
}

func newModule(path string, currentState state) *module {
	out := &lineWriter{w: os.Stdout}
	rpc := newRPC(out)
	return &module{
		path:     path,
		out:      out,
		rpc:      rpc,
		hc:       &hostCaller{rpc: rpc},
		state:    currentState,
		drafts:   make(map[int]*draft),
	}
}

func main() {
	path, err := statePath()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	current, err := loadState(path)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	m := newModule(path, current)
	// The stdin loop must never block while a request waits for a host
	// invoke: host.result frames arrive here and are the only way pending
	// invokes complete. Requests are handled on a worker goroutine instead.
	requests := make(chan []byte, 8)
	done := make(chan struct{})
	go m.serveRequests(requests, done)
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), maxLineBytes)
	for scanner.Scan() {
		line := append([]byte(nil), scanner.Bytes()...)
		if m.rpc.dispatchAsync(line) {
			continue
		}
		requests <- line
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
	// Drain already-buffered requests before exiting; otherwise responses to
	// the last frames would be lost when stdin closes.
	close(requests)
	<-done
}

func (m *module) serveRequests(requests <-chan []byte, done chan<- struct{}) {
	defer close(done)
	for line := range requests {
		var req request
		if err := json.Unmarshal(line, &req); err != nil {
			continue
		}
		data, err := encodeFrame(m.handle(req))
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			continue
		}
		if err := m.out.WriteLine(data); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	}
}

func statePath() (string, error) {
	if moduleStateDir := os.Getenv("LAVIS_MODULE_STATE_DIR"); moduleStateDir != "" {
		return filepath.Join(moduleStateDir, "state.json"), nil
	}
	if stateHome := os.Getenv("XDG_STATE_HOME"); stateHome != "" {
		return filepath.Join(stateHome, "lavis", "modules", "gaf", "state.json"), nil
	}
	executable, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("resolve executable: %w", err)
	}
	return filepath.Join(filepath.Dir(executable), "state.json"), nil
}

// loadState reads state.json and migrates v1 layouts (state_version absent)
// to v2 losslessly: scope defaults to everywhere-allowed and every trigger
// keeps its previous "fires in all chats" behavior via all_chats=true.
func loadState(path string) (state, error) {
	current := newState()
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		// v1 modules kept state.json next to the executable; the v2 module
		// uses LAVIS_MODULE_STATE_DIR, so fall back to the legacy location.
		if legacy := legacyExecutableStatePath(); legacy != "" && legacy != path {
			if legacyData, legacyErr := os.ReadFile(legacy); legacyErr == nil {
				data, err = legacyData, nil
			}
		}
	}
	if errors.Is(err, os.ErrNotExist) {
		return current, nil
	}
	if err != nil {
		return current, fmt.Errorf("read state: %w", err)
	}
	// Absent state_version means v1: reset the fresh-state default so
	// migrateState actually sees a legacy layout.
	current.StateVersion = 0
	if err := json.Unmarshal(data, &current); err != nil {
		return current, fmt.Errorf("decode state: %w", err)
	}
	migrateState(&current)
	if current.NextID < 1 {
		current.NextID = 1
	}
	if current.Active == nil {
		current.Active = make(map[string]activeEntry)
	}
	return current, nil
}

func legacyExecutableStatePath() string {
	executable, err := os.Executable()
	if err != nil {
		return ""
	}
	return filepath.Join(filepath.Dir(executable), "state.json")
}

func newState() state {
	return state{
		StateVersion: stateVersion,
		Enabled:      true,
		NextID:       1,
		Active:       make(map[string]activeEntry),
		Scope: scopeConfig{
			DMs:           true,
			Groups:        true,
			ChatOverrides: make(map[int64]bool),
		},
	}
}

func migrateState(value *state) {
	if value.StateVersion == stateVersion {
		if value.Scope.ChatOverrides == nil {
			value.Scope.ChatOverrides = make(map[int64]bool)
		}
		if value.Active == nil {
			value.Active = make(map[string]activeEntry)
		}
		return
	}
	// v1 state: no scope, no per-trigger flags. Preserve the historical
	// behavior — triggers fired everywhere a matching message appeared.
	if value.Scope.ChatOverrides == nil {
		value.Scope.ChatOverrides = make(map[int64]bool)
	}
	if value.Scope.DMs == false && value.Scope.Groups == false && len(value.Scope.ChatOverrides) == 0 {
		value.Scope.DMs = true
		value.Scope.Groups = true
	}
	for i := range value.Triggers {
		value.Triggers[i].AllChats = true
	}
	value.StateVersion = stateVersion
}

func (m *module) handle(req request) response {
	base := response{ProtocolVersion: protocolVersion, RequestID: req.RequestID}
	if req.ProtocolVersion != protocolVersion {
		base.Type = "error"
		base.Code = "PROTOCOL_VERSION"
		base.Message = "unsupported protocol version"
		return base
	}
	switch req.Type {
	case "initialize":
		base.Type = "initialized"
		base.ModuleID = req.ModuleID
	case "health":
		base.Type = "health"
	case "shutdown":
		os.Exit(0)
	case "execute":
		base.Type = "result"
		if req.Context != nil {
			m.context = req.Context
			m.chatID = req.Context.ChatID
		}
		ctx, cancel := context.WithTimeout(context.Background(), commandBudget)
		defer cancel()
		entities := []customEmojiEntity{}
		if req.Context != nil {
			entities = req.Context.ArgumentEntities
		}
		text, err := m.execute(ctx, req.Command, req.Arguments, entities)
		if err != nil {
			base.Type = "error"
			base.Code = "BAD_INPUT"
			base.Message = err.Error()
			fmt.Fprintf(os.Stderr, "execute %s %s: %v\n", req.Command, req.Arguments, err)
		} else {
			base.Text = &text
		}
	case "event":
		base.Type = "event_result"
		actions := m.handleEvent(req)
		if actions == nil {
			actions = []eventAction{}
		}
		base.Actions = &actions
	default:
		base.Type = "error"
		base.Code = "UNKNOWN_TYPE"
		base.Message = "unknown request type"
	}
	return base
}

func (m *module) execute(ctx context.Context, command, arguments string, entities []customEmojiEntity) (string, error) {
	command = strings.ToLower(strings.TrimSpace(command))
	if command == "gaf" {
		first, rest, restOffset := splitFirstWithOffset(arguments)
		if first == "" {
			return m.menuCommand(ctx)
		}
		command = strings.ToLower(first)
		arguments = rest
		entities = shiftEntities(entities, restOffset)
	}
	switch command {
	case "listt":
		return m.listText(), nil
	case "setr":
		if !strings.ContainsRune(arguments, '|') {
			return "", errors.New("использование: setr <триггер> | <реакция> [реакция] [реакция]")
		}
		return m.editorCommand(ctx, arguments, entities)
	case "remt":
		return m.remove(arguments)
	case "toggle":
		return m.toggle(arguments)
	default:
		return "", fmt.Errorf("неизвестная команда: %s", command)
	}
}

// menuCommand publishes the inline main menu through the host and deletes the
// owner's `,gaf` message. It mirrors the zai menuCommand pattern.
func (m *module) menuCommand(ctx context.Context) (string, error) {
	handle := m.messageHandle()
	if handle == "" {
		return "", errors.New("хост не передал message: inline-меню недоступно")
	}
	if err := m.hc.hostCall(ctx, "inline.form", map[string]any{
		"message": handle,
		"text":    m.menuText(),
		"buttons": m.mainMenuButtons(),
	}); err != nil {
		return "", fmt.Errorf("меню: %w", err)
	}
	_ = m.hc.hostCall(ctx, "message.deleteInvoker", map[string]any{"message": handle})
	return "", nil
}

func (m *module) messageHandle() string {
	if m.context == nil {
		return ""
	}
	return m.context.Message
}

// ---------- persistence ----------

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

func (m *module) saveEventState() {
	if err := m.save(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func (m *module) pruneActive() {
	for len(m.state.Active) > maxActiveMessages {
		oldestKey := ""
		oldestTime := int64(1<<63 - 1)
		for key, value := range m.state.Active {
			if value.SeenAt < oldestTime {
				oldestKey, oldestTime = key, value.SeenAt
			}
		}
		delete(m.state.Active, oldestKey)
	}
}

func (m *module) findTrigger(key string) int {
	if id, err := strconv.Atoi(key); err == nil {
		for i := range m.state.Triggers {
			if m.state.Triggers[i].ID == id {
				return i
			}
		}
		return -1
	}
	for i := range m.state.Triggers {
		if strings.EqualFold(m.state.Triggers[i].Word, key) {
			return i
		}
	}
	return -1
}

// ---------- textual commands ----------

func (m *module) listText() string {
	if len(m.state.Triggers) == 0 {
		return "📭 Триггеров нет. Добавьте: gaf setr никс | 👍"
	}
	triggers := append([]trigger(nil), m.state.Triggers...)
	sort.Slice(triggers, func(i, j int) bool { return triggers[i].ID < triggers[j].ID })
	var b strings.Builder
	b.WriteString("🎛 Триггеры GAF:\n")
	for _, item := range triggers {
		marker := "✅"
		if !item.Enabled {
			marker = "⏸"
		}
		scope := "🌐"
		if !item.AllChats {
			scope = "💬"
		}
		flags := ""
		if item.MatchStart {
			flags += "⇢"
		}
		if item.MatchEnd {
			flags += "⇠"
		}
		fmt.Fprintf(&b, "%s %d. %s | %s %s%s\n", marker, item.ID, item.Word, formatReactions(item.Reactions), scope, flags)
	}
	b.WriteString("\n⇢ agaf · ⇠ gafa · 🌐 все чаты · 💬 один чат")
	return strings.TrimSuffix(b.String(), "\n")
}

func (m *module) remove(arguments string) (string, error) {
	key := strings.TrimSpace(arguments)
	if key == "" {
		return "", errors.New("использование: remt <номер|слово>")
	}
	index := m.findTrigger(key)
	if index < 0 {
		return "", errors.New("триггер не найден")
	}
	removed := m.state.Triggers[index]
	m.state.Triggers = append(m.state.Triggers[:index], m.state.Triggers[index+1:]...)
	if err := m.save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("🗑 Триггер «%s» удалён", removed.Word), nil
}

func (m *module) toggle(arguments string) (string, error) {
	arguments = strings.TrimSpace(arguments)
	if arguments == "" || isSwitch(arguments) {
		value := !m.state.Enabled
		if arguments != "" {
			value = switchValue(arguments, value)
		}
		m.state.Enabled = value
		if err := m.save(); err != nil {
			return "", err
		}
		return fmt.Sprintf("GAF: %s", onOff(value)), nil
	}

	key := arguments
	requestedSwitch := ""
	index := m.findTrigger(key)
	if index < 0 {
		fields := strings.Fields(arguments)
		if len(fields) > 1 && isSwitch(fields[len(fields)-1]) {
			requestedSwitch = fields[len(fields)-1]
			key = strings.TrimSpace(strings.TrimSuffix(arguments, requestedSwitch))
			index = m.findTrigger(key)
		}
	}
	if key == "" {
		return "", errors.New("использование: toggle <номер|триггер> [on|off]")
	}
	if index < 0 {
		return "", errors.New("триггер не найден")
	}
	value := !m.state.Triggers[index].Enabled
	if requestedSwitch != "" {
		value = switchValue(requestedSwitch, value)
	}
	m.state.Triggers[index].Enabled = value
	if err := m.save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("Триггер «%s»: %s", m.state.Triggers[index].Word, onOff(value)), nil
}

// ---------- matching ----------

// scopeAllows resolves the per-chat override first; without one, positive
// dialog ids are private chats and negative ones are groups/channels.
func scopeAllows(scope scopeConfig, peerID int64) bool {
	if allowed, ok := scope.ChatOverrides[peerID]; ok {
		return allowed
	}
	if peerID > 0 {
		return scope.DMs
	}
	return scope.Groups
}

// containsTrigger matches word against text. By default the trigger must
// start at a word boundary and may continue inside a longer word (v1
// behavior: "фур" fires on "фури"). match_start (agaf) drops the leading
// boundary so mid-word matches like "агаф" fire; match_end (gafa) requires
// a trailing word boundary so only whole-word matches fire.
func containsTrigger(text, word string, matchStart, matchEnd bool) bool {
	textRunes := []rune(strings.ToLower(text))
	wordRunes := []rune(strings.ToLower(strings.TrimSpace(word)))
	if len(wordRunes) == 0 || len(wordRunes) > len(textRunes) {
		return false
	}
	for start := 0; start+len(wordRunes) <= len(textRunes); start++ {
		match := true
		for i := range wordRunes {
			if textRunes[start+i] != wordRunes[i] {
				match = false
				break
			}
		}
		if !match {
			continue
		}
		if !matchStart && start != 0 && isWordRune(textRunes[start-1]) {
			continue
		}
		end := start + len(wordRunes)
		if matchEnd && end != len(textRunes) && isWordRune(textRunes[end]) {
			continue
		}
		return true
	}
	return false
}

func (m *module) reactionsFor(text string, peerID int64) ([]reaction, string) {
	result := make([]reaction, 0, maxReactions)
	seen := make(map[string]struct{})
	note := ""
	for _, item := range m.state.Triggers {
		if !item.Enabled {
			continue
		}
		if !item.AllChats && item.ChatID != peerID {
			continue
		}
		if !containsTrigger(text, item.Word, item.MatchStart, item.MatchEnd) {
			continue
		}
		if note == "" {
			note = item.Word
		}
		for _, value := range item.Reactions {
			key := value.Type + "\x00" + value.Emoji + "\x00" + value.DocumentID
			if _, ok := seen[key]; ok {
				continue
			}
			seen[key] = struct{}{}
			result = append(result, value)
			if len(result) == maxReactions {
				return result, note
			}
		}
	}
	return result, note
}

// ---------- main menu ----------

func (m *module) menuText() string {
	status := "включён"
	if !m.state.Enabled {
		status = "выключен"
	}
	dm, gr := "❌", "❌"
	if m.state.Scope.DMs {
		dm = "✅"
	}
	if m.state.Scope.Groups {
		gr = "✅"
	}
	var b strings.Builder
	fmt.Fprintf(&b, "⚙️ GAF — %s\nЛС: %s · Группы: %s\nТриггеров: %d", status, dm, gr, len(m.state.Triggers))
	if m.chatID != 0 {
		chat := "❌"
		if scopeAllows(m.state.Scope, m.chatID) {
			chat = "✅"
		}
		fmt.Fprintf(&b, "\nЭтот чат: %s", chat)
	}
	return b.String()
}

func (m *module) mainMenuButtons() [][]inlineButton {
	dm, gr := "❌", "❌"
	if m.state.Scope.DMs {
		dm = "✅"
	}
	if m.state.Scope.Groups {
		gr = "✅"
	}
	first := []inlineButton{
		{Text: "ЛС: " + dm, Data: "d"},
		{Text: "Группы: " + gr, Data: "g"},
	}
	if m.chatID != 0 {
		chat := "❌"
		if scopeAllows(m.state.Scope, m.chatID) {
			chat = "✅"
		}
		first = append(first, inlineButton{Text: "Чат: " + chat, Data: "c"})
	}
	return [][]inlineButton{
		first,
		{{Text: "📋 Список", Data: "l"}},
		{{Text: "❌ Закрыть", Data: "x"}},
	}
}

func (m *module) listView() (string, [][]inlineButton) {
	if len(m.state.Triggers) == 0 {
		return "📭 Триггеров нет. Добавьте: ,gaf setr никс | 👍", [][]inlineButton{
			{{Text: "« Назад", Data: "b"}},
		}
	}
	triggers := append([]trigger(nil), m.state.Triggers...)
	sort.Slice(triggers, func(i, j int) bool { return triggers[i].ID < triggers[j].ID })
	var b strings.Builder
	b.WriteString("🎛 Триггеры GAF:\n")
	shown := 0
	hidden := 0
	for _, item := range triggers {
		entry := fmt.Sprintf("%s %d. %s | %s %s%s\n",
			onOffMark(item.Enabled), item.ID, item.Word, formatReactions(item.Reactions),
			listScopeMark(item.AllChats), listFlagMarks(item))
		if utf8.RuneCountInString(b.String())+utf8.RuneCountInString(entry) > maxListUTF16 {
			hidden = len(triggers) - shown
			break
		}
		b.WriteString(entry)
		shown++
	}
	if hidden > 0 {
		fmt.Fprintf(&b, "… и ещё %d\n", hidden)
	}
	b.WriteString("\n⇢ agaf · ⇠ gafa · 🌐 все чаты · 💬 один чат")
	return strings.TrimSuffix(b.String(), "\n"), [][]inlineButton{
		{{Text: "« Назад", Data: "b"}},
	}
}

func onOffMark(enabled bool) string {
	if enabled {
		return "✅"
	}
	return "⏸"
}

func listScopeMark(allChats bool) string {
	if allChats {
		return "🌐"
	}
	return "💬"
}

func listFlagMarks(item trigger) string {
	flags := ""
	if item.MatchStart {
		flags += "⇢"
	}
	if item.MatchEnd {
		flags += "⇠"
	}
	return flags
}

// ---------- trigger editor ----------

func (m *module) editorCommand(ctx context.Context, arguments string, entities []customEmojiEntity) (string, error) {
	word, reactionText, reactionOffset, err := splitTriggerRule(arguments)
	if err != nil {
		return "", err
	}
	if utf8.RuneCountInString(word) > maxTriggerRunes {
		return "", errors.New("триггер должен содержать от 1 до 64 символов")
	}
	reactions, err := parseReactions(tokenize(reactionText), shiftEntities(entities, reactionOffset))
	if err != nil {
		return "", err
	}
	handle := m.messageHandle()
	if handle == "" {
		return "", errors.New("хост не передал message: inline-меню недоступно")
	}
	d := &draft{Word: word, Reactions: reactions, AllChats: true, ChatID: m.chatID}
	for i := range m.state.Triggers {
		if strings.EqualFold(m.state.Triggers[i].Word, word) {
			existing := m.state.Triggers[i]
			d.Exists = true
			d.TriggerID = existing.ID
			d.MatchStart = existing.MatchStart
			d.MatchEnd = existing.MatchEnd
			d.AllChats = existing.AllChats
			if !existing.AllChats {
				d.ChatID = existing.ChatID
			}
			break
		}
	}
	id := m.storeDraft(d)
	if err := m.hc.hostCall(ctx, "inline.form", map[string]any{
		"message": handle,
		"text":    m.editorText(d),
		"buttons": m.editorButtons(id),
	}); err != nil {
		return "", fmt.Errorf("меню: %w", err)
	}
	_ = m.hc.hostCall(ctx, "message.deleteInvoker", map[string]any{"message": handle})
	return "", nil
}

func (m *module) storeDraft(d *draft) int {
	const maxDrafts = 16
	for len(m.drafts) >= maxDrafts {
		oldest := 0
		for id := range m.drafts {
			if oldest == 0 || id < oldest {
				oldest = id
			}
		}
		delete(m.drafts, oldest)
	}
	m.nextDraft++
	m.drafts[m.nextDraft] = d
	return m.nextDraft
}

func (m *module) editorText(d *draft) string {
	origin := "новый триггер"
	if d.Exists {
		origin = "существующий триггер — сохранение перезапишет"
	}
	scope := "все чаты"
	if !d.AllChats {
		scope = "только этот чат"
	}
	return fmt.Sprintf("✏️ %s | %s\nagaf: %s · gafa: %s\nОбласть: %s\n(%s)",
		d.Word, formatReactions(d.Reactions), onOff(d.MatchStart), onOff(d.MatchEnd), scope, origin)
}

func (m *module) editorButtons(id int) [][]inlineButton {
	d, ok := m.drafts[id]
	if !ok {
		return nil
	}
	startMark, endMark := "❌", "❌"
	if d.MatchStart {
		startMark = "✅"
	}
	if d.MatchEnd {
		endMark = "✅"
	}
	third := []inlineButton{{Text: "❌ Закрыть", Data: fmt.Sprintf("q%d", id)}}
	if d.Exists {
		third = []inlineButton{
			{Text: "✅ Сохранить", Data: fmt.Sprintf("v%d", id)},
			{Text: "🗑 Удалить", Data: fmt.Sprintf("r%d", id)},
			{Text: "❌ Закрыть", Data: fmt.Sprintf("q%d", id)},
		}
	}
	return [][]inlineButton{
		{
			{Text: "agaf: " + startMark, Data: fmt.Sprintf("s%d", id)},
			{Text: "✏️ Изменить", Data: fmt.Sprintf("w%d", id)},
			{Text: "gafa: " + endMark, Data: fmt.Sprintf("f%d", id)},
		},
		{
			{Text: "Все чаты", Data: fmt.Sprintf("a%d", id)},
			{Text: "Только этот чат", Data: fmt.Sprintf("o%d", id)},
		},
		third,
	}
}

// editorRow2 returns the scope radio row with the active choice marked.
func editorScopeButtons(id int, allChats bool) []inlineButton {
	all, one := "Все чаты", "Только этот чат"
	if allChats {
		all += " ✅"
	} else {
		one += " ✅"
	}
	return []inlineButton{
		{Text: all, Data: fmt.Sprintf("a%d", id)},
		{Text: one, Data: fmt.Sprintf("o%d", id)},
	}
}

// ---------- events ----------

func (m *module) handleEvent(req request) []eventAction {
	switch req.Event {
	case "bot.callback":
		var cb botCallback
		if len(req.Payload) > 0 {
			if err := json.Unmarshal(req.Payload, &cb); err != nil {
				fmt.Fprintf(os.Stderr, "bot.callback payload: %v\n", err)
				return []eventAction{}
			}
		}
		ctx, cancel := context.WithTimeout(context.Background(), commandBudget)
		defer cancel()
		m.handleCallback(ctx, cb)
		return []eventAction{}
	case "message.created", "message.edited":
		var payload eventPayload
		if len(req.Payload) > 0 {
			if err := json.Unmarshal(req.Payload, &payload); err != nil {
				return []eventAction{}
			}
		}
		return m.handleLifecycleEvent(req.Event, payload)
	}
	return []eventAction{}
}

func (m *module) handleCallback(ctx context.Context, cb botCallback) {
	// Acknowledge every press first so the client never shows a stuck
	// loading spinner, even when the follow-up action fails.
	_ = m.hc.hostCall(ctx, "inline.answer", map[string]any{
		"callback_id": cb.CallbackID,
		"text":        "",
		"show_alert":  false,
	})
	data := cb.Data
	if data == "" {
		return
	}
	switch data {
	case "m":
		m.redraw(ctx, cb, m.menuText(), m.mainMenuButtons())
	case "d":
		m.state.Scope.DMs = !m.state.Scope.DMs
		m.saveEventState()
		m.redraw(ctx, cb, m.menuText(), m.mainMenuButtons())
	case "g":
		m.state.Scope.Groups = !m.state.Scope.Groups
		m.saveEventState()
		m.redraw(ctx, cb, m.menuText(), m.mainMenuButtons())
	case "c":
		if m.chatID != 0 {
			m.state.Scope.ChatOverrides[m.chatID] = !scopeAllows(m.state.Scope, m.chatID)
			m.saveEventState()
		}
		m.redraw(ctx, cb, m.menuText(), m.mainMenuButtons())
	case "l":
		text, buttons := m.listView()
		m.redraw(ctx, cb, text, buttons)
	case "b":
		m.redraw(ctx, cb, m.menuText(), m.mainMenuButtons())
	case "x":
		m.closeMenu(ctx, cb)
	default:
		if len(data) < 2 {
			m.redraw(ctx, cb, "❓ Неизвестное действие", nil)
			return
		}
		m.handleEditorCallback(ctx, cb, data[0], data[1:])
	}
}

func (m *module) handleEditorCallback(ctx context.Context, cb botCallback, action byte, idText string) {
	id, err := strconv.Atoi(idText)
	if err != nil {
		m.redraw(ctx, cb, "❓ Неизвестное действие", nil)
		return
	}
	d := m.drafts[id]
	if d == nil {
		m.redraw(ctx, cb, "⏳ Меню устарело. Вызовите ,gaf заново", nil)
		return
	}
	switch action {
	case 'e':
		m.redraw(ctx, cb, m.editorText(d), m.editorButtons(id))
	case 's':
		d.MatchStart = !d.MatchStart
		m.redraw(ctx, cb, m.editorText(d), m.editorButtons(id))
	case 'f':
		d.MatchEnd = !d.MatchEnd
		m.redraw(ctx, cb, m.editorText(d), m.editorButtons(id))
	case 'a':
		d.AllChats = true
		m.redraw(ctx, cb, m.editorText(d), m.editorButtons(id))
	case 'o':
		d.AllChats = false
		m.redraw(ctx, cb, m.editorText(d), m.editorButtons(id))
	case 'w':
		m.waiting = &waiting{
			ChatID:          d.ChatID,
			DraftID:         id,
			InlineMessageID: cb.InlineMessageID,
			CallbackChatID:  cb.ChatID,
			CallbackMsgID:   cb.MessageID,
			ArmedAt:         time.Now(),
		}
		m.redraw(ctx, cb, "⏳ Пришли сообщением: слово | реакции\n(сообщение удалится и откроется меню)", nil)
	case 'v':
		text, err := m.saveDraft(d)
		if err != nil {
			m.redraw(ctx, cb, "❌ "+err.Error(), m.editorButtons(id))
			return
		}
		m.redraw(ctx, cb, text+"\n\nВыберите действие:", [][]inlineButton{
			editorScopeButtons(id, d.AllChats),
			{
				{Text: "« В меню", Data: "m"},
				{Text: "❌ Закрыть", Data: fmt.Sprintf("q%d", id)},
			},
		})
	case 'r':
		if !d.Exists {
			m.redraw(ctx, cb, "❌ Триггера ещё нет — сохраните его сначала", m.editorButtons(id))
			return
		}
		if index := m.findTrigger(strconv.Itoa(d.TriggerID)); index >= 0 {
			removed := m.state.Triggers[index]
			m.state.Triggers = append(m.state.Triggers[:index], m.state.Triggers[index+1:]...)
			if err := m.save(); err != nil {
				m.redraw(ctx, cb, "❌ "+err.Error(), m.editorButtons(id))
				return
			}
			d.Exists = false
			d.TriggerID = 0
			m.redraw(ctx, cb, fmt.Sprintf("🗑 Триггер «%s» удалён", removed.Word), m.editorButtons(id))
			return
		}
		m.redraw(ctx, cb, "❌ Триггер не найден", m.editorButtons(id))
	case 'q':
		delete(m.drafts, id)
		m.closeMenu(ctx, cb)
	default:
		m.redraw(ctx, cb, "❓ Неизвестное действие", nil)
	}
}

// saveDraft upserts the draft into persistent state: by trigger id when the
// draft edits an existing trigger, otherwise by word, otherwise as a new one.
func (m *module) saveDraft(d *draft) (string, error) {
	index := -1
	for i := range m.state.Triggers {
		if d.Exists && m.state.Triggers[i].ID == d.TriggerID {
			index = i
			break
		}
	}
	if index < 0 {
		for i := range m.state.Triggers {
			if strings.EqualFold(m.state.Triggers[i].Word, d.Word) {
				index = i
				break
			}
		}
	}
	scopeChat := int64(0)
	if !d.AllChats {
		scopeChat = d.ChatID
	}
	if index >= 0 {
		item := &m.state.Triggers[index]
		item.Word = d.Word
		item.Reactions = d.Reactions
		item.MatchStart = d.MatchStart
		item.MatchEnd = d.MatchEnd
		item.AllChats = d.AllChats
		item.ChatID = scopeChat
		item.Enabled = true
	} else {
		if len(m.state.Triggers) >= maxTriggers {
			return "", fmt.Errorf("достигнут лимит: %d триггеров", maxTriggers)
		}
		m.state.Triggers = append(m.state.Triggers, trigger{
			ID: m.state.NextID, Word: d.Word, Reactions: d.Reactions,
			Enabled: true, MatchStart: d.MatchStart, MatchEnd: d.MatchEnd,
			AllChats: d.AllChats, ChatID: scopeChat,
		})
		m.state.NextID++
		d.Exists = true
		d.TriggerID = m.state.Triggers[len(m.state.Triggers)-1].ID
	}
	if err := m.save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("✅ %s | %s", d.Word, formatReactions(d.Reactions)), nil
}

// redraw updates the menu message in place, preferring the inline message
// id that Bot API callbacks carry for via-bot menus (no query.message).
func (m *module) redraw(ctx context.Context, cb botCallback, text string, buttons [][]inlineButton) {
	params := map[string]any{"text": text}
	if len(buttons) > 0 {
		params["buttons"] = buttons
	} else {
		params["buttons"] = [][]inlineButton{}
	}
	if cb.InlineMessageID != "" {
		params["inline_message_id"] = cb.InlineMessageID
	} else {
		params["chat_id"] = cb.ChatID
		params["message_id"] = cb.MessageID
	}
	if err := m.hc.hostCall(ctx, "message.editBot", params); err != nil {
		fmt.Fprintf(os.Stderr, "message.editBot: %v\n", err)
	}
}

func (m *module) closeMenu(ctx context.Context, cb botCallback) {
	params := map[string]any{}
	if cb.InlineMessageID != "" {
		params["inline_message_id"] = cb.InlineMessageID
	} else {
		params["chat_id"] = cb.ChatID
		params["message_id"] = cb.MessageID
	}
	if err := m.hc.hostCall(ctx, "message.deleteBot", params); err == nil {
		return
	}
	m.redraw(ctx, cb, "🔒 Меню закрыто", [][]inlineButton{})
}

func (m *module) handleLifecycleEvent(event string, payload eventPayload) []eventAction {
	if !m.state.Enabled || payload.MessageRef == "" || payload.MessageKey == "" {
		return nil
	}
	if !scopeAllows(m.state.Scope, payload.PeerID) {
		return nil
	}
	if m.captureWaiting(event, payload) {
		return []eventAction{}
	}
	revision, _ := strconv.ParseUint(payload.EventID, 10, 64)
	previous, known := m.state.Active[payload.MessageKey]
	if known && revision != 0 && previous.Revision != 0 && revision <= previous.Revision {
		return nil
	}

	desired, note := m.reactionsFor(payload.Text, payload.PeerID)
	now := time.Now().Unix()
	entry := activeEntry{Reactions: desired, Revision: revision, SeenAt: now}
	if len(desired) == 0 {
		if event == "message.created" && !known {
			return nil
		}
		shouldRemove := event == "message.edited" && known && len(previous.Reactions) > 0
		m.state.Active[payload.MessageKey] = entry
		m.pruneActive()
		m.saveEventState()
		if !shouldRemove {
			return nil
		}
		return []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: []reaction{}}}
	}
	if known && equalReactions(previous.Reactions, desired) {
		m.state.Active[payload.MessageKey] = entry
		m.saveEventState()
		return nil
	}
	m.state.Active[payload.MessageKey] = entry
	m.pruneActive()
	m.saveEventState()
	return []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: desired, Note: note}}
}

// captureWaiting absorbs the next self-authored "word | reactions" message
// in the armed chat: the menu is republished from the new message's chat and
// the input message is deleted. Everything else in the armed window is fed
// back as a hint instead of triggering reactions.
func (m *module) captureWaiting(event string, payload eventPayload) bool {
	if m.waiting == nil {
		return false
	}
	if time.Since(m.waiting.ArmedAt) > waitingTTL {
		m.waiting = nil
		return false
	}
	if event != "message.created" || !payload.Outgoing || payload.PeerID != m.waiting.ChatID {
		return false
	}
	menu := botCallback{
		InlineMessageID: m.waiting.InlineMessageID,
		ChatID:          m.waiting.CallbackChatID,
		MessageID:       m.waiting.CallbackMsgID,
	}
	draftID := m.waiting.DraftID
	d := m.drafts[draftID]
	if d == nil {
		m.waiting = nil
		return false
	}
	word, reactionText, reactionOffset, err := splitTriggerRule(payload.Text)
	if err != nil {
		m.redraw(context.Background(), menu, "⏳ Формат: слово | реакции", nil)
		return true
	}
	if utf8.RuneCountInString(word) > maxTriggerRunes {
		m.redraw(context.Background(), menu, "⏳ Триггер длиннее 64 символов", nil)
		return true
	}
	reactions, err := parseReactions(tokenize(reactionText), shiftEntities(payload.Entities, reactionOffset))
	if err != nil {
		m.redraw(context.Background(), menu, "⏳ "+err.Error(), nil)
		return true
	}
	d.Word = word
	d.Reactions = reactions
	d.Exists = false
	d.TriggerID = 0
	for i := range m.state.Triggers {
		if strings.EqualFold(m.state.Triggers[i].Word, word) {
			existing := m.state.Triggers[i]
			d.Exists = true
			d.TriggerID = existing.ID
			d.MatchStart = existing.MatchStart
			d.MatchEnd = existing.MatchEnd
			d.AllChats = existing.AllChats
			if !existing.AllChats {
				d.ChatID = existing.ChatID
			}
			break
		}
	}
	if err := m.hc.hostCall(context.Background(), "inline.form", map[string]any{
		"message": payload.MessageRef,
		"text":    m.editorText(d),
		"buttons": m.editorButtons(draftID),
	}); err != nil {
		m.redraw(context.Background(), menu, "❌ Не удалось открыть меню: host inline.form", nil)
		m.waiting = nil
		return true
	}
	_ = m.hc.hostCall(context.Background(), "message.delete", map[string]any{"message": payload.MessageRef})
	m.waiting = nil
	return true
}

// ---------- parsing helpers (from v1) ----------

func splitTriggerRule(value string) (string, string, int, error) {
	pipe := strings.IndexRune(value, '|')
	if pipe < 0 {
		return "", "", 0, errors.New("использование: setr <триггер> | <реакция> [реакция] [реакция]")
	}
	word := strings.TrimSpace(value[:pipe])
	right := value[pipe+1:]
	reactionText := strings.TrimLeftFunc(right, unicode.IsSpace)
	if word == "" {
		return "", "", 0, errors.New("триггер слева от | не может быть пустым")
	}
	if strings.TrimSpace(reactionText) == "" {
		return "", "", 0, errors.New("укажите хотя бы одну реакцию справа от |")
	}
	offsetBytes := pipe + 1 + len(right) - len(reactionText)
	offsetUTF16 := len(utf16.Encode([]rune(value[:offsetBytes])))
	return word, strings.TrimSpace(reactionText), offsetUTF16, nil
}

func parseReactions(tokens []token, entities []customEmojiEntity) ([]reaction, error) {
	result := make([]reaction, 0, maxReactions)
	seen := make(map[string]struct{})
	for _, item := range tokens {
		var value reaction
		if entity, ok := overlappingEntity(item, entities); ok {
			if entity.DocumentID == "" || !allDigits(entity.DocumentID) {
				return nil, errors.New("некорректный Premium emoji document_id")
			}
			value = reaction{Type: "custom_emoji", DocumentID: entity.DocumentID}
		} else if hasCustomIDPrefix(item.Text) {
			id, ok := customID(item.Text)
			if !ok {
				return nil, errors.New("некорректный Premium emoji document_id")
			}
			value = reaction{Type: "custom_emoji", DocumentID: id}
		} else {
			if utf8.RuneCountInString(item.Text) > 32 {
				return nil, errors.New("обычная реакция слишком длинная")
			}
			value = reaction{Type: "emoji", Emoji: item.Text}
		}
		key := value.Type + "\x00" + value.Emoji + "\x00" + value.DocumentID
		if _, ok := seen[key]; ok {
			continue
		}
		seen[key] = struct{}{}
		result = append(result, value)
		if len(result) > maxReactions {
			return nil, fmt.Errorf("на одно сообщение разрешено максимум %d реакции", maxReactions)
		}
	}
	if len(result) == 0 {
		return nil, errors.New("укажите хотя бы одну реакцию")
	}
	return result, nil
}

func tokenize(value string) []token {
	var result []token
	utf16Offset := 0
	inToken := false
	startByte, startUTF16 := 0, 0
	for byteOffset, r := range value {
		width16 := len(utf16.Encode([]rune{r}))
		if unicode.IsSpace(r) {
			if inToken {
				result = append(result, token{Text: value[startByte:byteOffset], StartUTF16: startUTF16, EndUTF16: utf16Offset})
				inToken = false
			}
		} else if !inToken {
			inToken = true
			startByte, startUTF16 = byteOffset, utf16Offset
		}
		utf16Offset += width16
	}
	if inToken {
		result = append(result, token{Text: value[startByte:], StartUTF16: startUTF16, EndUTF16: utf16Offset})
	}
	return result
}

func overlappingEntity(item token, entities []customEmojiEntity) (customEmojiEntity, bool) {
	for _, entity := range entities {
		if entity.Type != "custom_emoji" {
			continue
		}
		end := entity.OffsetUTF16 + entity.LengthUTF16
		if entity.OffsetUTF16 < item.EndUTF16 && end > item.StartUTF16 {
			return entity, true
		}
	}
	return customEmojiEntity{}, false
}

func hasCustomIDPrefix(value string) bool {
	lower := strings.ToLower(value)
	return strings.HasPrefix(lower, "ce:") || strings.HasPrefix(lower, "custom:")
}

func customID(value string) (string, bool) {
	lower := strings.ToLower(value)
	for _, prefix := range []string{"ce:", "custom:"} {
		if strings.HasPrefix(lower, prefix) {
			id := value[len(prefix):]
			return id, id != "" && allDigits(id)
		}
	}
	return "", false
}

func allDigits(value string) bool {
	for _, r := range value {
		if r < '0' || r > '9' {
			return false
		}
	}
	return value != ""
}

func isWordRune(r rune) bool {
	return unicode.IsLetter(r) || unicode.IsNumber(r) || r == '_'
}

func splitFirstWithOffset(value string) (string, string, int) {
	trimmed := strings.TrimLeftFunc(value, unicode.IsSpace)
	leadingBytes := len(value) - len(trimmed)
	leadingUTF16 := len(utf16.Encode([]rune(value[:leadingBytes])))
	if trimmed == "" {
		return "", "", leadingUTF16
	}
	for i, r := range trimmed {
		if !unicode.IsSpace(r) {
			continue
		}
		tail := trimmed[i:]
		rest := strings.TrimLeftFunc(tail, unicode.IsSpace)
		spaceBytes := len(tail) - len(rest)
		offset := leadingUTF16 + len(utf16.Encode([]rune(trimmed[:i]))) + len(utf16.Encode([]rune(tail[:spaceBytes])))
		return trimmed[:i], strings.TrimSpace(rest), offset
	}
	return trimmed, "", leadingUTF16 + len(utf16.Encode([]rune(trimmed)))
}

func shiftEntities(entities []customEmojiEntity, offsetUTF16 int) []customEmojiEntity {
	shifted := make([]customEmojiEntity, 0, len(entities))
	for _, entity := range entities {
		if entity.OffsetUTF16 < offsetUTF16 {
			continue
		}
		entity.OffsetUTF16 -= offsetUTF16
		shifted = append(shifted, entity)
	}
	return shifted
}

func formatReactions(values []reaction) string {
	parts := make([]string, 0, len(values))
	for _, value := range values {
		if value.Type == "custom_emoji" {
			parts = append(parts, "Premium:"+value.DocumentID)
		} else {
			parts = append(parts, value.Emoji)
		}
	}
	return strings.Join(parts, " ")
}

func equalReactions(left, right []reaction) bool {
	if len(left) != len(right) {
		return false
	}
	for i := range left {
		if left[i] != right[i] {
			return false
		}
	}
	return true
}

func isSwitch(value string) bool {
	switch strings.ToLower(value) {
	case "on", "off", "вкл", "выкл":
		return true
	default:
		return false
	}
}

func switchValue(value string, fallback bool) bool {
	switch strings.ToLower(value) {
	case "on", "вкл":
		return true
	case "off", "выкл":
		return false
	default:
		return fallback
	}
}

func onOff(value bool) string {
	if value {
		return "включён"
	}
	return "выключен"
}
