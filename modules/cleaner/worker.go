package main

import (
	"context"
	"fmt"
	"math/rand"
	"os"
	"time"

	"github.com/gotd/td/tg"
)

const (
	syncInterval    = time.Hour
	cleanInterval   = 30 * time.Minute
	maxAge          = 12 * time.Hour
	batchSize       = 100
	pageLimit       = 100
	historyMaxPages = 50

	// getDialogs responses embed the top message of every dialog, so even a
	// small page can exceed the 64KB v6 line limit once base64-encoded.
	// Pages must stay tiny and results are persisted incrementally: a slow or
	// truncated sync still leaves a usable cache behind.
	dialogPageLimit = 10
	dialogMaxPages  = 30
)

// runBackground is the module's autonomous loop: a dialog-cache sync once an
// hour and a cleanup pass every 30 minutes. It never blocks lifecycle
// handling; RPC frames are emitted parentlessly through the raw transport,
// which is permitted by the v6 contract at any time.
func (m *module) runBackground() {
	ctx := context.Background()
	// The first sync runs immediately so commands can rely on a warm cache;
	// jitter only spaces out repeated passes after restarts.
	if err := m.syncDialogs(ctx); err != nil {
		fmt.Fprintln(os.Stderr, "sync:", err)
	}

	ticker := time.NewTicker(cleanInterval)
	defer ticker.Stop()
	for range ticker.C {
		var stale, enabled bool
		var selected int
		m.peekState(func(s *state) {
			stale = time.Since(time.Unix(s.LastSync, 0)) > syncInterval
			enabled = s.Enabled
			selected = len(s.Selected)
		})
		if stale {
			if err := m.syncDialogs(ctx); err != nil {
				fmt.Fprintln(os.Stderr, "sync:", err)
			}
		}
		if !enabled || selected == 0 {
			continue
		}
		// jitter avoids repeated restarts hitting Telegram at the same instant.
		time.Sleep(time.Duration(rand.Int63n(10)) * time.Second)
		if !m.beginClean() {
			continue
		}
		if err := m.cleanPass(ctx); err != nil {
			fmt.Fprintln(os.Stderr, "clean:", err)
		}
		m.endClean()
	}
}

// runNow starts an immediate cleanup pass without blocking the command:
// a full pass outlives the host's per-request deadline by design. The
// summary is delivered through the log topic and status as usual.
func (m *module) runNow() (string, error) {
	var selected int
	var enabled bool
	m.peekState(func(s *state) {
		selected = len(s.Selected)
		enabled = s.Enabled
	})
	if !enabled {
		return "", fmt.Errorf("cleaner выключен")
	}
	if selected == 0 {
		return "", fmt.Errorf("не выбрано ни одной группы: cleaner add <номер>")
	}
	if !m.beginClean() {
		return "", fmt.Errorf("прогон уже выполняется — подожди итог в лог-теме")
	}
	go func() {
		defer m.endClean()
		if err := m.cleanPass(context.Background()); err != nil {
			fmt.Fprintln(os.Stderr, "clean:", err)
		}
	}()
	return fmt.Sprintf("🧹 Прогон запущен: %d групп, критерий — свои сообщения старше 12 ч. Итог придёт в лог-тему Cleaner.", selected), nil
}

// syncDialogs refreshes the discovered-group cache by paging
// messages.getDialogs and persists it. Pages are small because the host
// aborts any single invoke that exceeds the v6 per-RPC deadline.
func (m *module) syncDialogs(ctx context.Context) error {
	var collected []groupEntry
	var offsetDate, offsetID int
	var offsetPeer tg.InputPeerClass = &tg.InputPeerEmpty{}
	pages := 0
	for pages < dialogMaxPages {
		body, err := m.call.call(ctx, &tg.MessagesGetDialogsRequest{
			OffsetDate: offsetDate,
			OffsetID:   offsetID,
			OffsetPeer: offsetPeer,
			Limit:      dialogPageLimit,
			Hash:       0,
		})
		if err != nil {
			if pages == 0 {
				return err
			}
			break
		}
		page, nextDate, nextID, nextPeer, hasMore, err := decodeDialogPage(body)
		if err != nil {
			if pages == 0 {
				return fmt.Errorf("decode dialogs: %w", err)
			}
			break
		}
		collected = append(collected, page...)
		pages++
		// Persist incrementally so a partial sync is still useful to
		// commands; offset paging repeats the boundary dialog, so dedupe by id.
		if err := m.withState(func(s *state) error {
			seenID := make(map[int64]bool)
			unique := make([]groupEntry, 0, len(collected))
			for _, entry := range collected {
				if seenID[entry.ID] {
					continue
				}
				seenID[entry.ID] = true
				unique = append(unique, entry)
			}
			s.Discovered = unique
			s.LastSync = time.Now().Unix()
			return nil
		}); err != nil {
			return err
		}
		if !hasMore || nextPeer == nil {
			break
		}
		offsetDate, offsetID, offsetPeer = nextDate, nextID, nextPeer
	}
	return nil
}

// decodeDialogPage returns the megagroup entries of one getDialogs page plus
// the offset (date/id/peer) of its last dialog for the next request.
func decodeDialogPage(body []byte) ([]groupEntry, int, int, tg.InputPeerClass, bool, error) {
	value, err := tg.DecodeMessagesDialogs(buffer(body))
	if err != nil {
		return nil, 0, 0, nil, false, err
	}
	var (
		chatClasses []tg.ChatClass
		dialogs     []tg.DialogClass
		hasMore     bool
	)
	switch d := value.(type) {
	case *tg.MessagesDialogs:
		chatClasses = d.GetChats()
		dialogs = d.GetDialogs()
		hasMore = len(dialogs) >= dialogPageLimit
	case *tg.MessagesDialogsSlice:
		chatClasses = d.GetChats()
		dialogs = d.GetDialogs()
		hasMore = len(dialogs) >= dialogPageLimit
	case *tg.MessagesDialogsNotModified:
		return nil, 0, 0, nil, false, fmt.Errorf("dialogs not modified")
	default:
		return nil, 0, 0, nil, false, fmt.Errorf("unexpected dialogs response %T", value)
	}
	accessHashes := make(map[int64]int64)
	for _, chat := range chatClasses {
		channel, ok := chat.(*tg.Channel)
		if !ok {
			continue
		}
		if hash, has := channel.GetAccessHash(); has {
			accessHashes[channel.GetID()] = hash
		}
	}
	userHashes := make(map[int64]int64)
	if withUsers, ok := value.(interface{ GetUsers() []tg.UserClass }); ok {
		for _, user := range withUsers.GetUsers() {
			concrete, ok := user.(*tg.User)
			if !ok {
				continue
			}
			if hash, has := concrete.GetAccessHash(); has {
				userHashes[concrete.GetID()] = hash
			}
		}
	}
	var entries []groupEntry
	for _, chat := range chatClasses {
		channel, ok := chat.(*tg.Channel)
		if !ok || !channel.Megagroup {
			continue
		}
		accessHash, hasHash := channel.GetAccessHash()
		if !hasHash || accessHash == 0 {
			continue
		}
		entries = append(entries, groupEntry{
			ID:         channel.GetID(),
			AccessHash: accessHash,
			Title:      channel.GetTitle(),
			Forum:      channel.Forum,
		})
	}
	if len(dialogs) == 0 {
		return entries, 0, 0, nil, false, nil
	}
	messageDates := make(map[int]int)
	if withMessages, ok := value.(interface{ GetMessages() []tg.MessageClass }); ok {
		for _, message := range withMessages.GetMessages() {
			if concrete, ok := message.(*tg.Message); ok {
				messageDates[concrete.GetID()] = concrete.GetDate()
			}
		}
	}
	last := dialogs[len(dialogs)-1]
	var peer tg.PeerClass
	var date, id int
	switch d := last.(type) {
	case *tg.Dialog:
		peer, id = d.Peer, d.TopMessage
		if found, has := messageDates[id]; has {
			date = found
		}
	case *tg.DialogFolder:
		peer, id = d.Peer, d.TopMessage
		if found, has := messageDates[id]; has {
			date = found
		}
	default:
		return entries, 0, 0, nil, false, nil
	}
	// dialog.peer is a bare Peer; the paging offset needs an InputPeer with
	// the matching access hash resolved from the same response.
	var next tg.InputPeerClass
	switch p := peer.(type) {
	case *tg.PeerChat:
		next = &tg.InputPeerChat{ChatID: p.ChatID}
	case *tg.PeerChannel:
		hash, known := accessHashes[p.ChannelID]
		if !known {
			return entries, 0, 0, nil, false, nil
		}
		next = &tg.InputPeerChannel{ChannelID: p.ChannelID, AccessHash: hash}
	case *tg.PeerUser:
		hash, known := userHashes[p.UserID]
		if !known {
			return entries, 0, 0, nil, false, nil
		}
		next = &tg.InputPeerUser{UserID: p.UserID, AccessHash: hash}
	default:
		return entries, 0, 0, nil, false, nil
	}
	return entries, date, id, next, hasMore, nil
}

// cleanPass removes the account's own messages older than maxAge in every
// selected group and reports a single summary to the log topic.
func (m *module) cleanPass(ctx context.Context) error {
	var groups []groupEntry
	m.peekState(func(s *state) {
		groups = append([]groupEntry(nil), s.Selected...)
	})
	totalDeleted := 0
	for _, group := range groups {
		deleted, err := m.cleanGroup(ctx, group)
		if err != nil {
			fmt.Fprintln(os.Stderr, "clean:", group.Title, err)
			continue
		}
		totalDeleted += deleted
	}
	if err := m.withState(func(s *state) error {
		s.LastRun = time.Now().Unix()
		return nil
	}); err != nil {
		fmt.Fprintln(os.Stderr, "state:", err)
	}

	if totalDeleted > 0 {
		m.logMessage(fmt.Sprintf("🧹 Cleaner: удалено сообщений старше 12 ч: %d", totalDeleted))
	}
	return nil
}

// cleanGroup deletes the account's own messages older than maxAge in one
// group and returns the count. messages.search from_id filtering proved
// unreliable through the raw transport (it returned whole-chat pages), so
// the history is walked explicitly and every candidate is verified
// client-side: author == self, date older than cutoff. Foreign and service
// messages are never selected.
func (m *module) cleanGroup(ctx context.Context, group groupEntry) (int, error) {
	peer := &tg.InputPeerChannel{ChannelID: group.ID, AccessHash: group.AccessHash}
	cutoff := int(time.Now().Add(-maxAge).Unix())

	deleted := 0
	seen := make(map[int]bool)
	offsetID := 0
	selfID := m.selfID.Load()
	for page := 0; page < historyMaxPages; page++ {
		body, err := m.call.call(ctx, &tg.MessagesGetHistoryRequest{
			Peer:       peer,
			OffsetID:   offsetID,
			OffsetDate: 0,
			AddOffset:  0,
			Limit:      pageLimit,
			MaxID:      0,
			MinID:      0,
			Hash:       0,
		})
		if err != nil {
			return deleted, fmt.Errorf("history: %w", err)
		}
		messages, users, err := decodeHistoryPage(body)
		if err != nil {
			return deleted, fmt.Errorf("decode history: %w", err)
		}
		if len(messages) == 0 {
			break
		}
		if selfID == 0 {
			selfID = findSelfID(users)
			if selfID == 0 {
				return deleted, fmt.Errorf("self user not present in history response")
			}
			m.selfID.Store(selfID)
		}
		ids, minID := ownDeletable(messages, selfID, cutoff, seen)
		for start := 0; start < len(ids); start += batchSize {
			end := start + batchSize
			if end > len(ids) {
				end = len(ids)
			}
			batch := ids[start:end]
			channel := &tg.InputChannel{ChannelID: group.ID, AccessHash: group.AccessHash}
			if err := m.deleteBatch(ctx, channel, batch); err != nil {
				return deleted, fmt.Errorf("deleteMessages: %w", err)
			}
			deleted += len(batch)
			time.Sleep(500 * time.Millisecond)
		}
		if minID == 0 || (offsetID != 0 && minID >= offsetID) {
			break
		}
		offsetID = minID
		if len(messages) < pageLimit {
			break
		}
	}
	return deleted, nil
}

// ownDeletable selects ids of real messages authored by selfID and older
// than cutoff, and reports the smallest message id on the page used to
// advance the history cursor.
func ownDeletable(messages []tg.MessageClass, selfID int64, cutoff int, seen map[int]bool) ([]int, int) {
	var ids []int
	minID := 0
	for _, message := range messages {
		id := message.GetID()
		if id == 0 {
			continue
		}
		if minID == 0 || id < minID {
			minID = id
		}
		concrete, ok := message.(*tg.Message)
		if !ok {
			continue
		}
		value, has := concrete.GetFromID()
		from, ok := value.(*tg.PeerUser)
		if !has || !ok || from.UserID != selfID {
			continue
		}
		date := concrete.GetDate()
		if date == 0 || date >= cutoff {
			continue
		}
		if seen[id] {
			continue
		}
		seen[id] = true
		ids = append(ids, id)
	}
	return ids, minID
}

func findSelfID(users []tg.UserClass) int64 {
	for _, user := range users {
		concrete, ok := user.(*tg.User)
		if !ok {
			continue
		}
		if concrete.GetSelf() {
			return concrete.GetID()
		}
	}
	return 0
}

func decodeHistoryPage(body []byte) ([]tg.MessageClass, []tg.UserClass, error) {
	value, err := tg.DecodeMessagesMessages(buffer(body))
	if err != nil {
		return nil, nil, err
	}
	switch m := value.(type) {
	case *tg.MessagesMessages:
		return m.GetMessages(), m.GetUsers(), nil
	case *tg.MessagesMessagesSlice:
		return m.GetMessages(), m.GetUsers(), nil
	case *tg.MessagesChannelMessages:
		return m.GetMessages(), m.GetUsers(), nil
	case *tg.MessagesMessagesNotModified:
		return nil, nil, nil
	default:
		return nil, nil, fmt.Errorf("unexpected messages response %T", value)
	}
}

// deleteBatch revokes messages through channels.deleteMessages with an
// explicit InputChannel. messages.deleteMessages resolves bare message ids
// on the target DC and silently returns pts_count=0 when the ids are not
// peer-encoded, so it cannot be used safely from a raw transport.
func (m *module) deleteBatch(ctx context.Context, channel tg.InputChannelClass, ids []int) error {
	_, err := m.call.call(ctx, &tg.ChannelsDeleteMessagesRequest{Channel: channel, ID: ids})
	return err
}
