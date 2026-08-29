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
	syncInterval  = time.Hour
	cleanInterval = 30 * time.Minute
	maxAge        = 12 * time.Hour
	batchSize     = 100
	pageLimit     = 100

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
		if err := m.cleanPass(ctx); err != nil {
			fmt.Fprintln(os.Stderr, "clean:", err)
		}
	}
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
// group and returns the count.
func (m *module) cleanGroup(ctx context.Context, group groupEntry) (int, error) {
	peer := &tg.InputPeerChannel{ChannelID: group.ID, AccessHash: group.AccessHash}
	cutoff := int(time.Now().Add(-maxAge).Unix())

	deleted := 0
	seen := make(map[int]bool)
	offsetID := 0
	for page := 0; page < 50; page++ {
		body, err := m.call.call(ctx, &tg.MessagesSearchRequest{
			Peer:      peer,
			Q:         "",
			FromID:    &tg.InputPeerSelf{},
			Filter:    &tg.InputMessagesFilterEmpty{},
			MaxDate:   cutoff,
			OffsetID:  offsetID,
			AddOffset: 0,
			Limit:     pageLimit,
			MaxID:     0,
			MinID:     0,
			Hash:      0,
		})
		if err != nil {
			return deleted, fmt.Errorf("search: %w", err)
		}
		messages, err := decodeSearchMessages(body)
		if err != nil {
			return deleted, fmt.Errorf("decode search: %w", err)
		}
		if len(messages) == 0 {
			break
		}
		var ids []int
		for _, message := range messages {
			id := message.GetID()
			if id == 0 || seen[id] {
				continue
			}
			seen[id] = true
			ids = append(ids, id)
		}
		for start := 0; start < len(ids); start += batchSize {
			end := start + batchSize
			if end > len(ids) {
				end = len(ids)
			}
			batch := ids[start:end]
			if err := m.deleteBatch(ctx, batch); err != nil {
				return deleted, fmt.Errorf("deleteMessage: %w", err)
			}
			deleted += len(batch)
			time.Sleep(500 * time.Millisecond)
		}
		if len(messages) < pageLimit {
			break
		}
		if len(ids) == 0 {
			break
		}
		offsetID = ids[len(ids)-1]
	}
	return deleted, nil
}

func decodeSearchMessages(body []byte) ([]tg.MessageClass, error) {
	value, err := tg.DecodeMessagesMessages(buffer(body))
	if err != nil {
		return nil, err
	}
	switch m := value.(type) {
	case *tg.MessagesMessages:
		return m.GetMessages(), nil
	case *tg.MessagesMessagesSlice:
		return m.GetMessages(), nil
	case *tg.MessagesChannelMessages:
		return m.GetMessages(), nil
	case *tg.MessagesMessagesNotModified:
		return nil, nil
	default:
		return nil, fmt.Errorf("unexpected messages response %T", value)
	}
}

func (m *module) deleteBatch(ctx context.Context, ids []int) error {
	_, err := m.call.call(ctx, &tg.MessagesDeleteMessagesRequest{Revoke: true, ID: ids})
	return err
}
