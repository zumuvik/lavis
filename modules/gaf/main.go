package main

import (
	"bufio"
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
	protocolVersion   = 4
	maxReactions      = 3
	maxTriggers       = 128
	maxActiveMessages = 4096
)

type customEmojiEntity struct {
	Type        string `json:"type"`
	OffsetUTF16 int    `json:"offset_utf16"`
	LengthUTF16 int    `json:"length_utf16"`
	DocumentID  string `json:"document_id"`
}

type requestContext struct {
	ArgumentEntities []customEmojiEntity `json:"argument_entities"`
}

type eventPayload struct {
	EventID    string              `json:"event_id"`
	MessageRef string              `json:"message_ref"`
	MessageKey string              `json:"message_key"`
	Text       string              `json:"text"`
	Outgoing   bool                `json:"outgoing"`
	Entities   []customEmojiEntity `json:"entities"`
}

type request struct {
	ProtocolVersion int            `json:"protocol_version"`
	Type            string         `json:"type"`
	RequestID       string         `json:"request_id"`
	ModuleID        string         `json:"module_id"`
	Command         string         `json:"command"`
	Arguments       string         `json:"arguments"`
	Context         requestContext `json:"context"`
	Event           string         `json:"event"`
	Payload         eventPayload   `json:"payload"`
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
}

type response struct {
	ProtocolVersion int            `json:"protocol_version"`
	Type            string         `json:"type"`
	RequestID       string         `json:"request_id"`
	ModuleID        string         `json:"module_id,omitempty"`
	Text            string         `json:"text,omitempty"`
	Code            string         `json:"code,omitempty"`
	Message         string         `json:"message,omitempty"`
	Actions         *[]eventAction `json:"actions,omitempty"`
}

type trigger struct {
	ID        int        `json:"id"`
	Word      string     `json:"word"`
	Reactions []reaction `json:"reactions"`
	Enabled   bool       `json:"enabled"`
}

type activeEntry struct {
	Reactions []reaction `json:"reactions"`
	Revision  uint64     `json:"-"`
	SeenAt    int64      `json:"seen_at"`
}

type state struct {
	Enabled  bool                   `json:"enabled"`
	NextID   int                    `json:"next_id"`
	Triggers []trigger              `json:"triggers"`
	Active   map[string]activeEntry `json:"active"`
}

type token struct {
	Text       string
	StartUTF16 int
	EndUTF16   int
}

type module struct {
	path  string
	state state
}

func main() {
	module, err := loadModule()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), 64*1024)
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	for scanner.Scan() {
		var req request
		if err := json.Unmarshal(scanner.Bytes(), &req); err != nil {
			continue
		}
		resp := module.handle(req)
		if err := encoder.Encode(resp); err != nil {
			fmt.Fprintln(os.Stderr, err)
			return
		}
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func loadModule() (*module, error) {
	path, legacyPath, err := statePath()
	if err != nil {
		return nil, err
	}
	current := state{Enabled: true, NextID: 1, Active: make(map[string]activeEntry)}
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) && legacyPath != "" {
		data, err = os.ReadFile(legacyPath)
	}
	if err == nil {
		if err := json.Unmarshal(data, &current); err != nil {
			return nil, fmt.Errorf("decode state: %w", err)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read state: %w", err)
	}
	if current.NextID < 1 {
		current.NextID = 1
	}
	if current.Active == nil {
		current.Active = make(map[string]activeEntry)
	}
	return &module{path: path, state: current}, nil
}

func statePath() (string, string, error) {
	if moduleStateDir := os.Getenv("LAVIS_MODULE_STATE_DIR"); moduleStateDir != "" {
		legacyPath, err := legacyExecutableStatePath()
		return filepath.Join(moduleStateDir, "state.json"), legacyPath, err
	}
	if stateHome := os.Getenv("XDG_STATE_HOME"); stateHome != "" {
		legacyPath, err := legacyExecutableStatePath()
		return filepath.Join(stateHome, "lavis", "modules", "gaf", "state.json"), legacyPath, err
	}
	legacyPath, err := legacyExecutableStatePath()
	if err != nil {
		return "", "", err
	}
	return legacyPath, "", nil
}

func legacyExecutableStatePath() (string, error) {
	executable, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("resolve executable: %w", err)
	}
	return filepath.Join(filepath.Dir(executable), "state.json"), nil
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
		text, err := m.execute(req.Command, req.Arguments, req.Context.ArgumentEntities)
		if err != nil {
			base.Type = "error"
			base.Code = "BAD_INPUT"
			base.Message = err.Error()
		} else {
			base.Text = text
		}
	case "event":
		base.Type = "event_result"
		actions := m.handleEvent(req.Event, req.Payload)
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

func (m *module) execute(command, arguments string, entities []customEmojiEntity) (string, error) {
	command = strings.ToLower(strings.TrimSpace(command))
	if command == "gaf" {
		first, rest, restOffset := splitFirstWithOffset(arguments)
		if first == "" {
			return m.menu(), nil
		}
		command = strings.ToLower(first)
		arguments = rest
		entities = shiftEntities(entities, restOffset)
	}
	switch command {
	case "listt":
		return m.list(), nil
	case "setr":
		return m.set(arguments, entities)
	case "remt":
		return m.remove(arguments)
	case "toggle":
		return m.toggle(arguments)
	default:
		return "", fmt.Errorf("неизвестная команда: %s", command)
	}
}

func (m *module) menu() string {
	status := "включён"
	if !m.state.Enabled {
		status = "выключен"
	}
	return fmt.Sprintf("⚙️ GAF — %s\nТриггеров: %d\n\nКоманды:\n• gaf listt\n• gaf setr <триггер> | <1–3 реакции>\n• gaf remt <номер|слово>\n• gaf toggle [on|off|номер|слово]\n\nPremium-эмодзи можно вставить справа от `|` прямо в setr.", status, len(m.state.Triggers))
}

func (m *module) list() string {
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
		fmt.Fprintf(&b, "%s %d. %s | %s\n", marker, item.ID, item.Word, formatReactions(item.Reactions))
	}
	return strings.TrimSuffix(b.String(), "\n")
}

func (m *module) set(arguments string, entities []customEmojiEntity) (string, error) {
	word, reactionText, reactionOffset, err := splitTriggerRule(arguments)
	if err != nil {
		return "", err
	}
	if utf8.RuneCountInString(word) > 64 {
		return "", errors.New("триггер должен содержать от 1 до 64 символов")
	}
	reactions, err := parseReactions(tokenize(reactionText), shiftEntities(entities, reactionOffset))
	if err != nil {
		return "", err
	}
	index := -1
	for i := range m.state.Triggers {
		if strings.EqualFold(m.state.Triggers[i].Word, word) {
			index = i
			break
		}
	}
	if index >= 0 {
		m.state.Triggers[index].Word = word
		m.state.Triggers[index].Reactions = reactions
		m.state.Triggers[index].Enabled = true
	} else {
		if len(m.state.Triggers) >= maxTriggers {
			return "", fmt.Errorf("достигнут лимит: %d триггеров", maxTriggers)
		}
		m.state.Triggers = append(m.state.Triggers, trigger{
			ID:        m.state.NextID,
			Word:      word,
			Reactions: reactions,
			Enabled:   true,
		})
		m.state.NextID++
	}
	if err := m.save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("✅ %s | %s", word, formatReactions(reactions)), nil
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

func (m *module) handleEvent(event string, payload eventPayload) []eventAction {
	if !m.state.Enabled || payload.MessageRef == "" || payload.MessageKey == "" {
		return nil
	}
	revision, _ := strconv.ParseUint(payload.EventID, 10, 64)
	previous, known := m.state.Active[payload.MessageKey]
	if known && revision != 0 && previous.Revision != 0 && revision <= previous.Revision {
		return nil
	}

	desired := m.reactionsFor(payload.Text)
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
	return []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: desired}}
}

func (m *module) saveEventState() {
	if err := m.save(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func (m *module) reactionsFor(text string) []reaction {
	result := make([]reaction, 0, maxReactions)
	seen := make(map[string]struct{})
	for _, item := range m.state.Triggers {
		if !item.Enabled || !containsWord(text, item.Word) {
			continue
		}
		for _, value := range item.Reactions {
			key := value.Type + "\x00" + value.Emoji + "\x00" + value.DocumentID
			if _, ok := seen[key]; ok {
				continue
			}
			seen[key] = struct{}{}
			result = append(result, value)
			if len(result) == maxReactions {
				return result
			}
		}
	}
	return result
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

func containsWord(text, word string) bool {
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
		beforeOK := start == 0 || !isWordRune(textRunes[start-1])
		end := start + len(wordRunes)
		afterOK := end == len(textRunes) || !isWordRune(textRunes[end])
		if beforeOK && afterOK {
			return true
		}
	}
	return false
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
