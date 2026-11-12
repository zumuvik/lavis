package main

import (
	"bytes"
	"context"
	"fmt"
	"strconv"
	"strings"

	"github.com/gotd/td/bin"
	"github.com/gotd/td/tg"
)

func rawBuffer(body []byte) *bin.Buffer {
	b := new(bin.Buffer)
	b.ResetTo(body)
	return b
}

// The locally pinned gotd schema predates these two Telegram user/channel
// constructor IDs; the fields match the forms used by Lavis' cleaner module.
func normalizeConstructors(body []byte) []byte {
	body = bytes.ReplaceAll(body, []byte{0x1c, 0xb1, 0x32, 0x1c}, []byte{0xc6, 0x34, 0x9f, 0xd4})
	return bytes.ReplaceAll(body, []byte{0x88, 0x43, 0x77, 0x31}, []byte{0x83, 0xcc, 0xb8, 0xb1})
}

func (m *module) rawCall(ctx context.Context, request bin.Encoder) ([]byte, error) {
	if m.raw != nil {
		return m.raw(ctx, request)
	}
	var buf bin.Buffer
	if err := request.Encode(&buf); err != nil {
		return nil, err
	}
	return m.rawInvoke(ctx, buf.Copy())
}

func inputFromPeer(peer tg.PeerClass, users []tg.UserClass, chats []tg.ChatClass) tg.InputPeerClass {
	switch p := peer.(type) {
	case *tg.PeerUser:
		for _, u := range users {
			if user, ok := u.(*tg.User); ok && user.ID == p.UserID {
				if hash, ok := user.GetAccessHash(); ok && hash != 0 {
					return &tg.InputPeerUser{UserID: p.UserID, AccessHash: hash}
				}
			}
		}
	case *tg.PeerChat:
		return &tg.InputPeerChat{ChatID: p.ChatID}
	case *tg.PeerChannel:
		for _, c := range chats {
			if channel, ok := c.(*tg.Channel); ok && channel.ID == p.ChannelID {
				if hash, ok := channel.GetAccessHash(); ok && hash != 0 {
					return &tg.InputPeerChannel{ChannelID: p.ChannelID, AccessHash: hash}
				}
			}
		}
	}
	return nil
}

func (m *module) resolve(ctx context.Context, target string) (tg.InputPeerClass, error) {
	if strings.HasPrefix(target, "@") {
		body, err := m.rawCall(ctx, &tg.ContactsResolveUsernameRequest{Username: strings.TrimPrefix(target, "@")})
		if err != nil {
			return nil, err
		}
		var found tg.ContactsResolvedPeer
		if err := found.Decode(rawBuffer(body)); err != nil {
			return nil, fmt.Errorf("resolve username: %w", err)
		}
		if peer := inputFromPeer(found.Peer, found.Users, found.Chats); peer != nil {
			return peer, nil
		}
		return nil, fmt.Errorf("username unavailable: %s", target)
	}
	// Telegram IDs alone do not contain the access hash needed for input peers.
	// Fetch it from dialogs; never substitute a zero access hash for a user/channel.
	id, err := parseDialogID(target)
	if err != nil {
		return nil, err
	}
	var offsetPeer tg.InputPeerClass = &tg.InputPeerEmpty{}
	var offsetDate, offsetID int
	for page := 0; page < 20; page++ {
		body, err := m.rawCall(ctx, &tg.MessagesGetDialogsRequest{
			OffsetDate: offsetDate, OffsetID: offsetID, OffsetPeer: offsetPeer, Limit: 100,
		})
		if err != nil {
			return nil, err
		}
		value, err := tg.DecodeMessagesDialogs(rawBuffer(body))
		if err != nil {
			return nil, fmt.Errorf("decode dialogs: %w", err)
		}
		var dialogs []tg.DialogClass
		var users []tg.UserClass
		var chats []tg.ChatClass
		var messages []tg.MessageClass
		switch d := value.(type) {
		case *tg.MessagesDialogs:
			dialogs, users, chats, messages = d.Dialogs, d.Users, d.Chats, d.Messages
		case *tg.MessagesDialogsSlice:
			dialogs, users, chats, messages = d.Dialogs, d.Users, d.Chats, d.Messages
		default:
			return nil, fmt.Errorf("unexpected dialogs response %T", value)
		}
		for _, d := range dialogs {
			peer := dialogPeer(d)
			if peer != nil && dialogID(peer) == id {
				if input := inputFromPeer(peer, users, chats); input != nil {
					return input, nil
				}
				return nil, fmt.Errorf("dialog %s has no access hash", target)
			}
		}
		if len(dialogs) < 100 {
			break
		}
		last := dialogs[len(dialogs)-1]
		offsetPeer = inputFromPeer(dialogPeer(last), users, chats)
		if offsetPeer == nil {
			break
		}
		offsetID = dialogTopMessage(last)
		offsetDate = 0
		for _, message := range messages {
			if msg, ok := message.(*tg.Message); ok && msg.ID == offsetID {
				offsetDate = msg.Date
			}
		}
	}
	return nil, fmt.Errorf("dialog %s not found among accessible dialogs", target)
}

func dialogPeer(dialog tg.DialogClass) tg.PeerClass {
	switch d := dialog.(type) {
	case *tg.Dialog:
		return d.Peer
	case *tg.DialogFolder:
		return d.Peer
	default:
		return nil
	}
}
func dialogTopMessage(dialog tg.DialogClass) int {
	switch d := dialog.(type) {
	case *tg.Dialog:
		return d.TopMessage
	case *tg.DialogFolder:
		return d.TopMessage
	default:
		return 0
	}
}
func dialogID(peer tg.PeerClass) int64 {
	switch p := peer.(type) {
	case *tg.PeerUser:
		return p.UserID
	case *tg.PeerChat:
		return -p.ChatID
	case *tg.PeerChannel:
		return -1_000_000_000_000 - p.ChannelID
	default:
		return 0
	}
}

func parseDialogID(target string) (int64, error) {
	id, err := strconv.ParseInt(target, 10, 64)
	if err != nil || id == 0 {
		return 0, fmt.Errorf("invalid dialog ID: %s", target)
	}
	return id, nil
}
