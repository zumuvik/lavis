package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

// commandBudget must terminate before the host's 5s lifecycle deadline so a
// slow command replies with an error text instead of being SIGKILLed mid-request.
const commandBudget = 4 * time.Second

func main() {
	module, err := loadModule()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	// The stdin loop must never block while a command waits for an RPC:
	// telegram.result frames arrive here and are the only way pending
	// invokes complete. Requests are handled on a worker goroutine instead.
	requests := make(chan []byte, 8)
	done := make(chan struct{})
	go module.serveRequests(requests, done)
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), maxLineBytes)
	for scanner.Scan() {
		line := append([]byte(nil), scanner.Bytes()...)
		if module.rpc.dispatchAsync(line) {
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

// lineWriter serializes every outbound frame. Response and parentless
// telegram.invoke frames share one stdout pipe, and a frame larger than
// PIPE_BUF is not written atomically; a torn newline corrupts both sides.
type lineWriter struct {
	mu sync.Mutex
	w  io.Writer
}

// WriteLine emits one complete JSON line under the lock. Write exists so the
// writer satisfies io.Writer, but callers must pass a whole line: a frame
// larger than PIPE_BUF is not written atomically, and a torn newline
// corrupts both sides of the transport.
func (l *lineWriter) Write(data []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.w.Write(data)
}

func (l *lineWriter) WriteLine(data []byte) error {
	l.mu.Lock()
	defer l.mu.Unlock()
	_, err := l.w.Write(append(data, '\n'))
	return err
}

func encodeFrame(value any) ([]byte, error) {
	var buf bytes.Buffer
	encoder := json.NewEncoder(&buf)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimRight(buf.Bytes(), "\n"), nil
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
		ctx, cancel := context.WithTimeout(context.Background(), commandBudget)
		defer cancel()
		text, err := m.execute(ctx, req.Command, req.Arguments)
		if err != nil {
			base.Type = "error"
			base.Code = "BAD_INPUT"
			base.Message = err.Error()
			fmt.Fprintf(os.Stderr, "execute %s %s: %v\n", req.Command, req.Arguments, err)
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

func (m *module) execute(ctx context.Context, command, arguments string) (string, error) {
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
			return m.logText(ctx)
		default:
			return m.statusText()
		}
	}
	return m.statusText()
}

// listGroups prints the discovered-group cache with numbered entries.
// Entry 0 is a special "add all" target; entries 1..n map to cached chats.
func (m *module) listGroups() (string, error) {
	var cache []groupEntry
	selected := make(map[int64]bool)
	m.peekState(func(s *state) {
		cache = append([]groupEntry(nil), s.Discovered...)
		for _, group := range s.Selected {
			selected[group.ID] = true
		}
	})
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
	var addedCount int
	var message string
	err := m.withState(func(s *state) error {
		resolved, err := resolveNumber(s, key)
		if err != nil {
			return err
		}
		already := make(map[int64]bool)
		for _, group := range s.Selected {
			already[group.ID] = true
		}
		addedCount = 0
		for _, entry := range resolved {
			if already[entry.ID] {
				continue
			}
			s.Selected = append(s.Selected, groupEntry{
				ID:         entry.ID,
				AccessHash: entry.AccessHash,
				Title:      entry.Title,
			})
			already[entry.ID] = true
			addedCount++
		}
		if addedCount == 0 {
			message = "ℹ️ Ничего не добавлено: все выбранные группы уже в списке."
			return nil
		}
		message = fmt.Sprintf("✅ Добавлено групп: %d. Всего в чистке: %d.", addedCount, len(s.Selected))
		return nil
	})
	if err != nil {
		return "", err
	}
	if addedCount > 0 {
		m.logf("➕ Добавлены группы для чистки: %d", addedCount)
	}
	return message, nil
}

func (m *module) removeGroups(key string) (string, error) {
	var removed groupEntry
	var message string
	err := m.withState(func(s *state) error {
		index, err := strconv.Atoi(key)
		if err != nil || index < 1 {
			return fmt.Errorf("номер должен быть положительным числом")
		}
		if index > len(s.Selected) {
			return fmt.Errorf("нет группы с номером %d (выбрано: %d)", index, len(s.Selected))
		}
		removed = s.Selected[index-1]
		s.Selected = append(s.Selected[:index-1], s.Selected[index:]...)
		message = fmt.Sprintf("🗑 Группа «%s» исключена. Осталось: %d.", removed.Title, len(s.Selected))
		return nil
	})
	if err != nil {
		return "", err
	}
	m.logf("➖ Группа исключена из чистки: %s", removed.Title)
	return message, nil
}

func (m *module) statusText() (string, error) {
	var b strings.Builder
	m.peekState(func(s *state) {
		b.WriteString("🧹 Cleaner: чистка своих сообщений старше 12 часов\n")
		if !s.Enabled {
			b.WriteString("Состояние: ⏸ выключен\n")
		} else {
			fmt.Fprintf(&b, "Состояние: ✅ включён (последний прогон %s)\n", humanTime(s.LastRun))
		}
		fmt.Fprintf(&b, "Выбрано групп: %d\n", len(s.Selected))
		for i, group := range s.Selected {
			fmt.Fprintf(&b, "  %d. %s\n", i+1, group.Title)
		}
		if s.LogTopicID != 0 {
			fmt.Fprintf(&b, "Лог-тема: Cleaner #%d\n", s.LogTopicID)
		} else {
			b.WriteString("Лог-тема: ещё не найдена (cleaner log)\n")
		}
	})
	b.WriteString(
		"\nКоманды: cleaner list, cleaner add <номер>, " +
			"cleaner remove <номер>, cleaner log, cleaner status",
	)
	return strings.TrimSuffix(b.String(), "\n"), nil
}

func (m *module) logText(ctx context.Context) (string, error) {
	if err := m.ensureLogTopic(ctx); err != nil {
		return "", fmt.Errorf("лог-тема недоступна: %w", err)
	}
	var ref topicRef
	m.peekState(func(s *state) {
		ref = topicRef{chatID: s.LogChatID, topicID: s.LogTopicID}
	})
	return fmt.Sprintf("📋 Лог-тема Cleaner активна: %s (topic #%d)", ref.location(), ref.topicID), nil
}

func resolveNumber(s *state, key string) ([]groupEntry, error) {
	index, err := strconv.Atoi(key)
	if err != nil {
		return nil, fmt.Errorf("<номер> должен быть числом")
	}
	if index == 0 {
		return append([]groupEntry(nil), s.Discovered...), nil
	}
	if index < 1 || index > len(s.Discovered) {
		return nil, fmt.Errorf("нет группы с номером %d (всего: %d)", index, len(s.Discovered))
	}
	entry := s.Discovered[index-1]
	return []groupEntry{entry}, nil
}

func humanTime(unix int64) string {
	if unix == 0 {
		return "никогда"
	}
	return time.Unix(unix, 0).Format("02.01 15:04")
}
