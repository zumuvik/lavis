package main

import (
	"context"
	"fmt"
	"math/rand"
	"time"

	"github.com/gotd/td/tg"
)

const (
	companionGroupTitle = "Lavis"
	logTopicTitle       = "Cleaner"
)

// topicRef is a lock-free snapshot of the log destination.
type topicRef struct {
	chatID     int64
	accessHash int64
	topicID    int
}

func (r topicRef) valid() bool {
	return r.chatID != 0 && r.accessHash != 0 && r.topicID != 0
}

func (r topicRef) location() string {
	if r.chatID == 0 {
		return "группа Lavis"
	}
	return fmt.Sprintf("группа Lavis (%d)", r.chatID)
}

// ensureLogTopic resolves the Lavis companion group from the dialog cache,
// then finds or creates the Cleaner topic in it. RPCs run outside the state
// lock; only the final assignment is persisted atomically.
func (m *module) ensureLogTopic(ctx context.Context) error {
	var cached, forumFound bool
	var entry groupEntry
	var cache []groupEntry
	m.peekState(func(s *state) {
		cached = s.LogChatID != 0 && s.LogAccessHash != 0 && s.LogTopicID != 0
		cache = append([]groupEntry(nil), s.Discovered...)
	})
	if cached {
		return nil
	}
	if len(cache) == 0 {
		// The command budget is shorter than a cold getDialogs round trip;
		// the background sync warms the cache right after startup instead.
		return fmt.Errorf("кэш диалогов ещё не готов — попробуй cleaner log через 15 секунд")
	}
	for _, candidate := range cache {
		if candidate.Title == companionGroupTitle {
			entry = candidate
			forumFound = true
			break
		}
	}
	if !forumFound {
		return fmt.Errorf("companion group %q not found", companionGroupTitle)
	}
	if !entry.Forum {
		return fmt.Errorf("companion group %s is not a forum", entry.Title)
	}
	topicID, err := m.findOrCreateTopic(ctx, &entry)
	if err != nil {
		return err
	}
	return m.withState(func(s *state) error {
		s.LogChatID = entry.ID
		s.LogAccessHash = entry.AccessHash
		s.LogTopicID = topicID
		s.LogTopicMarker = entry.Title
		return nil
	})
}

func (m *module) findOrCreateTopic(ctx context.Context, group *groupEntry) (int, error) {
	peer := &tg.InputPeerChannel{ChannelID: group.ID, AccessHash: group.AccessHash}

	if topicID, err := m.findTopicID(ctx, peer); err == nil {
		return topicID, nil
	}

	// gotd/td does not generate the messages.forumTopic constructor for the
	// create response, so the id is read back through a short topic search.
	// The small sleep keeps this inside the command budget while giving the
	// search index a chance to observe the new topic.
	if _, err := m.call.call(ctx, &tg.MessagesCreateForumTopicRequest{
		Peer:     peer,
		Title:    logTopicTitle,
		RandomID: rand.Int63(),
	}); err != nil {
		return 0, fmt.Errorf("createForumTopic: %w", err)
	}
	select {
	case <-time.After(300 * time.Millisecond):
	case <-ctx.Done():
		return 0, ctx.Err()
	}
	return m.findTopicID(ctx, peer)
}

func (m *module) findTopicID(ctx context.Context, peer *tg.InputPeerChannel) (int, error) {
	body, err := m.call.call(ctx, &tg.MessagesGetForumTopicsRequest{
		Peer:        peer,
		Q:           logTopicTitle,
		OffsetDate:  0,
		OffsetID:    0,
		OffsetTopic: 0,
		Limit:       50,
	})
	if err != nil {
		return 0, fmt.Errorf("getForumTopics: %w", err)
	}
	return decodeForumTopics(body)
}

func decodeForumTopics(body []byte) (int, error) {
	var value tg.MessagesForumTopics
	if err := value.DecodeBare(buffer(body)); err != nil {
		return 0, fmt.Errorf("decode forum topics: %w", err)
	}
	for _, topic := range value.GetTopics() {
		concrete, ok := topic.(*tg.ForumTopic)
		if !ok || concrete.GetTitle() != logTopicTitle {
			continue
		}
		return concrete.GetID(), nil
	}
	return 0, fmt.Errorf("topic %q not found", logTopicTitle)
}

// postToTopic sends one message into a previously resolved topic. It never
// touches m.state, so it is safe to run while the state lock is held by a
// command handler.
func (m *module) postToTopic(ctx context.Context, ref topicRef, text string) error {
	if !ref.valid() {
		return fmt.Errorf("log topic is not configured")
	}
	peer := &tg.InputPeerChannel{
		ChannelID:  ref.chatID,
		AccessHash: ref.accessHash,
	}
	_, err := m.call.call(ctx, &tg.MessagesSendMessageRequest{
		Peer:     peer,
		ReplyTo:  &tg.InputReplyToMessage{ReplyToMsgID: ref.topicID},
		Message:  text,
		RandomID: rand.Int63(),
	})
	return err
}
