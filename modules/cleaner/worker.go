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

	// getDialogs responses travel as one 64KB-capped JSON line; 200 dialogs
	// with access hashes can exceed it, so the page is deliberately smaller.
	dialogPageLimit = 100
)

// runBackground is the module's autonomous loop: a dialog-cache sync once an
// hour and a cleanup pass every 30 minutes. It never blocks lifecycle
// handling; RPC frames are emitted parentlessly through the raw transport,
// which is permitted by the v6 contract at any time.
func (m *module) runBackground() {
	ctx := context.Background()
	// jitter avoids repeated restarts hitting Telegram at the same instant.
	time.Sleep(3*time.Second + time.Duration(rand.Int63n(7))*time.Second)

	if err := m.syncDialogs(ctx, true); err != nil {
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
			if err := m.syncDialogs(ctx, true); err != nil {
				fmt.Fprintln(os.Stderr, "sync:", err)
			}
		}
		if !enabled || selected == 0 {
			continue
		}
		if err := m.cleanPass(ctx); err != nil {
			fmt.Fprintln(os.Stderr, "clean:", err)
		}
	}
}

// syncDialogs refreshes the discovered-group cache via messages.getDialogs
// and persists it.
func (m *module) syncDialogs(ctx context.Context, silent bool) error {
	body, err := m.call.call(ctx, &tg.MessagesGetDialogsRequest{
		OffsetDate: 0,
		OffsetID:   0,
		OffsetPeer: &tg.InputPeerEmpty{},
		Limit:      dialogPageLimit,
		Hash:       0,
	})
	if err != nil {
		if silent {
			return nil
		}
		return fmt.Errorf("getDialogs: %w", err)
	}
	dialogs, err := decodeDialogs(body)
	if err != nil {
		return fmt.Errorf("decode dialogs: %w", err)
	}
	return m.withState(func(s *state) error {
		s.Discovered = dialogs
		s.LastSync = time.Now().Unix()
		return nil
	})
}

func decodeDialogs(body []byte) ([]groupEntry, error) {
	value, err := tg.DecodeMessagesDialogs(buffer(body))
	if err != nil {
		return nil, err
	}
	var chats []tg.ChatClass
	switch d := value.(type) {
	case *tg.MessagesDialogs:
		chats = d.GetChats()
	case *tg.MessagesDialogsSlice:
		chats = d.GetChats()
	case *tg.MessagesDialogsNotModified:
		return nil, fmt.Errorf("dialogs not modified")
	default:
		return nil, fmt.Errorf("unexpected dialogs response %T", value)
	}
	var entries []groupEntry
	for _, chat := range chats {
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
	return entries, nil
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
