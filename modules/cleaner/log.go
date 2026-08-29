package main

import (
	"fmt"
	"math/rand"
	"time"

	"github.com/gotd/td/tg"
)

const (
	companionGroupTitle = "Lavis"
	logTopicTitle       = "Cleaner"
)

// ensureLogTopic resolves the Lavis companion group from the dialog cache,
// then finds or creates the Cleaner topic in it. Results are persisted.
func (m *module) ensureLogTopic() error {
	if m.state.LogChatID != 0 && m.state.LogAccessHash != 0 && m.state.LogTopicID != 0 {
		return nil
	}
	if len(m.state.Discovered) == 0 {
		if err := m.syncDialogs(false); err != nil {
			return err
		}
	}
	for i := range m.state.Discovered {
		entry := &m.state.Discovered[i]
		if entry.Title != companionGroupTitle {
			continue
		}
		if !entry.Forum {
			return fmt.Errorf("companion group %s is not a forum", entry.Title)
		}
		m.state.LogChatID = entry.ID
		m.state.LogAccessHash = entry.AccessHash
		topicID, err := m.findOrCreateTopic(entry)
		if err != nil {
			return err
		}
		m.state.LogTopicID = topicID
		m.state.LogTopicMarker = entry.Title
		return m.save()
	}
	return fmt.Errorf("companion group %q not found", companionGroupTitle)
}

func (m *module) findOrCreateTopic(group *groupEntry) (int, error) {
	peer := &tg.InputPeerChannel{ChannelID: group.ID, AccessHash: group.AccessHash}

	topicID, err := m.findTopicID(peer)
	if err == nil {
		return topicID, nil
	}

	if _, err := m.call.call(&tg.MessagesCreateForumTopicRequest{
		Peer:     peer,
		Title:    logTopicTitle,
		RandomID: rand.Int63(),
	}); err != nil {
		return 0, fmt.Errorf("createForumTopic: %w", err)
	}
	time.Sleep(500 * time.Millisecond)
	return m.findTopicID(peer)
}

func (m *module) findTopicID(peer *tg.InputPeerChannel) (int, error) {
	body, err := m.call.call(&tg.MessagesGetForumTopicsRequest{
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

// sendLogMessage posts one message into the Cleaner topic.
func (m *module) sendLogMessage(text string) error {
	if m.state.LogChatID == 0 || m.state.LogTopicID == 0 {
		return fmt.Errorf("log topic is not configured")
	}
	peer := &tg.InputPeerChannel{
		ChannelID:  m.state.LogChatID,
		AccessHash: m.state.LogAccessHash,
	}
	_, err := m.call.call(&tg.MessagesSendMessageRequest{
		Peer:     peer,
		ReplyTo:  &tg.InputReplyToMessage{ReplyToMsgID: m.state.LogTopicID},
		Message:  text,
		RandomID: rand.Int63(),
	})
	return err
}
