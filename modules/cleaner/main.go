package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
)

func main() {
	module, err := loadModule()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), maxLineBytes)
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	for scanner.Scan() {
		line := scanner.Bytes()
		if module.rpc.dispatchAsync(line) {
			continue
		}
		var req request
		if err := json.Unmarshal(line, &req); err != nil {
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

type request struct {
	ProtocolVersion int    `json:"protocol_version"`
	Type            string `json:"type"`
	RequestID       string `json:"request_id"`
	ModuleID        string `json:"module_id"`
	Command         string `json:"command"`
	Arguments       string `json:"arguments"`
	Event           string `json:"event"`
}

type response struct {
	ProtocolVersion int    `json:"protocol_version"`
	Type            string `json:"type"`
	RequestID       string `json:"request_id"`
	ModuleID        string `json:"module_id,omitempty"`
	Text            string `json:"text,omitempty"`
	Code            string `json:"code,omitempty"`
	Message         string `json:"message,omitempty"`
	Actions         *[]any `json:"actions,omitempty"`
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
		go m.runBackground()
	case "health":
		base.Type = "health"
	case "shutdown":
		os.Exit(0)
	case "execute":
		base.Type = "result"
		text, err := m.execute(req.Command, req.Arguments)
		if err != nil {
			base.Type = "error"
			base.Code = "BAD_INPUT"
			base.Message = err.Error()
		} else {
			base.Text = text
		}
	case "event":
		base.Type = "event_result"
		empty := []any{}
		base.Actions = &empty
	default:
		base.Type = "error"
		base.Code = "UNKNOWN_TYPE"
		base.Message = "unknown request type"
	}
	return base
}

func (m *module) execute(command, arguments string) (string, error) {
	command = strings.ToLower(strings.TrimSpace(command))
	args := strings.Fields(arguments)
	for _, arg := range args {
		arg = strings.ToLower(arg)
		switch arg {
		case "list":
			return m.listGroups()
		case "add":
			if len(args) < 2 {
				return "", fmt.Errorf("использование: cleaner add <номер>")
			}
			return m.addGroups(args[1])
		case "remove":
			if len(args) < 2 {
				return "", fmt.Errorf("использование: cleaner remove <номер>")
			}
			return m.removeGroups(args[1])
		case "status":
			return m.statusText()
		case "log":
			return m.logText()
		default:
			return m.statusText()
		}
	}
	return m.statusText()
}

// listGroups prints the discovered dialog cache with numbered entries.
// Entry 0 is a special "add all" target; entries 1..n map to cached chats.
func (m *module) listGroups() (string, error) {
	cache := m.state.Discovered
	selected := make(map[int64]bool)
	for _, group := range m.state.Selected {
		selected[group.ID] = true
	}
	var b strings.Builder
	b.WriteString("🧹 Cleaner: группы с сообщениями старше 12 ч\n\n")
	b.WriteString("0. Добавить все\n")
	for i, entry := range cache {
		marker := "  "
		if selected[entry.ID] {
			marker = "✅"
		}
		fmt.Fprintf(&b, "%s%d. %s\n", marker, i+1, entry.Title)
	}
	if len(cache) == 0 {
		b.WriteString("\nКэш групп пуст. Фоновая синхронизация ещё не закончилась — попробуйте позже.\n")
	}
	b.WriteString("\nКоманды: cleaner add <номер>, cleaner remove <номер>, cleaner status, cleaner log")
	return strings.TrimSuffix(b.String(), "\n"), nil
}

func (m *module) addGroups(key string) (string, error) {
	resolved, err := m.resolveNumber(key)
	if err != nil {
		return "", err
	}
	already := make(map[int64]bool)
	for _, group := range m.state.Selected {
		already[group.ID] = true
	}
	addedCount := 0
	for _, entry := range resolved {
		if already[entry.ID] {
			continue
		}
		m.state.Selected = append(m.state.Selected, groupEntry{
			ID:         entry.ID,
			AccessHash: entry.AccessHash,
			Title:      entry.Title,
		})
		already[entry.ID] = true
		addedCount++
	}
	if err := m.save(); err != nil {
		return "", err
	}
	if addedCount == 0 {
		return "ℹ️ Ничего не добавлено: все выбранные группы уже в списке.", nil
	}
	m.logf("➕ Добавлены группы для чистки: %d", addedCount)
	return fmt.Sprintf("✅ Добавлено групп: %d. Всего в чистке: %d.", addedCount, len(m.state.Selected)), nil
}

func (m *module) removeGroups(key string) (string, error) {
	index, err := strconv.Atoi(key)
	if err != nil || index < 1 {
		return "", fmt.Errorf("номер должен быть положительным числом")
	}
	if index > len(m.state.Selected) {
		return "", fmt.Errorf("нет группы с номером %d (выбрано: %d)", index, len(m.state.Selected))
	}
	removed := m.state.Selected[index-1]
	m.state.Selected = append(m.state.Selected[:index-1], m.state.Selected[index-1:]...)
	if err := m.save(); err != nil {
		return "", err
	}
	m.logf("➖ Группа исключена из чистки: %s", removed.Title)
	return fmt.Sprintf("🗑 Группа «%s» исключена. Осталось: %d.", removed.Title, len(m.state.Selected)), nil
}

func (m *module) statusText() (string, error) {
	var b strings.Builder
	b.WriteString("🧹 Cleaner: чистка своих сообщений старше 12 часов\n")
	if !m.state.Enabled {
		b.WriteString("Состояние: ⏸ выключен\n")
	} else {
		fmt.Fprintf(&b, "Состояние: ✅ включён (последний прогон %s)\n", humanTime(m.state.LastRun))
	}
	fmt.Fprintf(&b, "Выбрано групп: %d\n", len(m.state.Selected))
	for i, group := range m.state.Selected {
		fmt.Fprintf(&b, "  %d. %s\n", i+1, group.Title)
	}
	if m.state.LogTopicID != 0 {
		fmt.Fprintf(&b, "Лог-тема: Cleaner #%d\n", m.state.LogTopicID)
	} else {
		b.WriteString("Лог-тема: ещё не найдена (cleaner log)\n")
	}
	b.WriteString(
		"\nКоманды: cleaner list, cleaner add <номер>, " +
			"cleaner remove <номер>, cleaner log, cleaner status",
	)
	return strings.TrimSuffix(b.String(), "\n"), nil
}

func (m *module) logText() (string, error) {
	if err := m.ensureLogTopic(); err != nil {
		return "", fmt.Errorf("лог-тема недоступна: %w", err)
	}
	return fmt.Sprintf("📋 Лог-тема Cleaner активна: %s (topic #%d)", m.logTopicLocation(), m.state.LogTopicID), nil
}

func (m *module) resolveNumber(key string) ([]groupEntry, error) {
	index, err := strconv.Atoi(key)
	if err != nil {
		return nil, fmt.Errorf("<номер> должен быть числом")
	}
	if index == 0 {
		return append([]groupEntry(nil), m.state.Discovered...), nil
	}
	if index < 1 || index > len(m.state.Discovered) {
		return nil, fmt.Errorf("нет группы с номером %d (всего: %d)", index, len(m.state.Discovered))
	}
	entry := m.state.Discovered[index-1]
	return []groupEntry{entry}, nil
}

func humanTime(unix int64) string {
	if unix == 0 {
		return "никогда"
	}
	return time.Unix(unix, 0).Format("02.01 15:04")
}

func (m *module) logTopicLocation() string {
	if m.state.LogChatID == 0 {
		return "группа Lavis"
	}
	return fmt.Sprintf("группа Lavis (%d)", m.state.LogChatID)
}
