package main

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/gotd/td/tg"
)

const (
	opsecPageLimit = 25
	opsecMaxPages  = 4000
	opsecMaxIDs    = 5000
)

type opsecChat struct {
	RawID      int64  `json:"raw_id"`
	AccessHash int64  `json:"access_hash"`
	Title      string `json:"title"`
	Count      int    `json:"count"`
	Newest     int64  `json:"newest"`
	Oldest     int64  `json:"oldest"`
	NewestMsg  int    `json:"newest_msg,omitempty"`
	InDialog   bool   `json:"in_dialog"`
	IDs        []int  `json:"ids,omitempty"`
}

type opsecState struct {
	ScannedAt int64       `json:"scanned_at,omitempty"`
	Chats     []opsecChat `json:"chats,omitempty"`
}

var opsecPhases = []string{
	"▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n██ F·S0CIETY  ACCESS DENIED ██\n▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n> handshake telegram.mtproto ......... OK\n> exploiting message index ........... GO\n> сканирую твои цифровые следы...\n\nповтори cleaner opsec для прогресса",
	"█▓▒░ fsociety ░▒▓█\n> decrypting dialog shards [####------]\n> index@telegram:~$ grep -r you /history\n> чем больше сообщений, тем ближе кверху",
	"█▓▒░ fsociety ░▒▓█\n> decrypting dialog shards [########--]\n> index@telegram:~$ wc -l your_regretz\n> hello, friend",
	"█▓▒░ fsociety ░▒▓█\n> decrypting dialog shards [##########]\n> почти готово, ещё один заход...",
}

// opsecCommand handles `cleaner opsec` (scan/report) and
// `cleaner opsec add <номер>` (purge selection, 0 = all ghosts).
func (m *module) opsecCommand(ctx context.Context, args []string) (string, error) {
	if len(args) > 1 && strings.EqualFold(args[1], "add") {
		if len(args) < 3 {
			return "", fmt.Errorf("использование: cleaner opsec add <номер>")
		}
		return m.opsecPurge(args[2])
	}
	return m.opsecReport()
}

func (m *module) opsecReport() (string, error) {
	var opsec *opsecState
	m.peekState(func(s *state) { opsec = s.Opsec })
	scanning := m.opsecRunning.Load()
	if scanning {
		if opsec == nil || opsec.ScannedAt == 0 {
			phase := int(m.opsecPhase.Add(1) % uint64(len(opsecPhases)))
			return opsecPhases[phase], nil
		}
	}
	if opsec == nil || opsec.ScannedAt == 0 {
		if !m.opsecRunning.CompareAndSwap(false, true) {
			return "… скан уже запущен, подожди", nil
		}
		go m.runOpsec()
		return opsecPhases[0], nil
	}

	byCount := sortedOpsec(opsec.Chats)
	var b strings.Builder
	b.WriteString("▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄\n")
	b.WriteString("██ F·S0CIETY  ACCESS GRANTED ██\n")
	b.WriteString("▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n")
	ghosts, ghostMsgs := 0, 0
	for _, c := range byCount {
		if !c.InDialog {
			ghosts++
			ghostMsgs += c.Count
		}
	}
	fmt.Fprintf(&b, "целей: %d, призраков: %d (%d сообщений в чатах без тебя)\n\n", len(byCount), ghosts, ghostMsgs)
	if ghosts > 0 {
		b.WriteString("0. ⛏ ВСЕ ПРИЗРАКИ — cleaner opsec add 0\n")
	}
	limit := len(byCount)
	if limit > 25 {
		limit = 25
	}
	for i := 0; i < limit; i++ {
		c := byCount[i]
		marker := ""
		if !c.InDialog {
			marker = " 👻"
		}
		fmt.Fprintf(&b, "%d. %s — %d сообщ. (посл. %s)%s\n", i+1, opsecLink(c), c.Count, humanTime(c.Newest), marker)
	}
	if len(byCount) > limit {
		fmt.Fprintf(&b, "… и ещё %d целей ниже\n", len(byCount)-limit)
	}
	b.WriteString("\nзачистка: cleaner opsec add <номер>")
	return strings.TrimSuffix(b.String(), "\n"), nil
}

func sortedOpsec(chats []opsecChat) []opsecChat {
	out := append([]opsecChat(nil), chats...)
	sort.SliceStable(out, func(i, j int) bool { return out[i].Count > out[j].Count })
	return out
}

// opsecLink points at the newest own message so the chat opens with one
// click directly onto your own footprint.
func opsecLink(c opsecChat) string {
	title := c.Title
	if title == "" {
		title = fmt.Sprintf("chat %d", c.RawID)
	}
	if c.NewestMsg == 0 || c.RawID == 0 {
		return fmt.Sprintf("«%s»", title)
	}
	return fmt.Sprintf("https://t.me/c/%d/%d «%s»", c.RawID, c.NewestMsg, title)
}

func (m *module) opsecPurge(key string) (string, error) {
	var opsec *opsecState
	m.peekState(func(s *state) { opsec = s.Opsec })
	if opsec == nil || len(opsec.Chats) == 0 {
		return "", fmt.Errorf("сначала сканируй: cleaner opsec")
	}
	index, err := strconv.Atoi(key)
	if err != nil || index < 0 {
		return "", fmt.Errorf("номер должен быть числом ≥ 0")
	}
	byCount := sortedOpsec(opsec.Chats)
	var targets []opsecChat
	if index == 0 {
		for _, c := range byCount {
			if !c.InDialog {
				targets = append(targets, c)
			}
		}
		if len(targets) == 0 {
			return "", fmt.Errorf("призраков нет — скан чист")
		}
	} else {
		if index > len(byCount) {
			return "", fmt.Errorf("нет цели с номером %d (всего: %d)", index, len(byCount))
		}
		targets = []opsecChat{byCount[index-1]}
	}
	if !m.beginPurge() {
		return "", fmt.Errorf("зачистка уже выполняется")
	}
	go func() {
		defer m.endPurge()
		m.runOpsecPurge(context.Background(), targets)
	}()
	var names []string
	total := 0
	for _, t := range targets {
		names = append(names, fmt.Sprintf("«%s»", t.Title))
		total += t.Count
	}
	if len(names) > 3 {
		names = append(names[:3], fmt.Sprintf("и ещё %d", len(targets)-3))
	}
	return fmt.Sprintf("💣 зачистка началась: %s (~%d сообщений). Итог — в лог-теме.", strings.Join(names, ", "), total), nil
}

func (m *module) runOpsecPurge(ctx context.Context, targets []opsecChat) {
	deleted := 0
	chatsDone := 0
	var failed []string
	for _, t := range targets {
		channel := &tg.InputChannel{ChannelID: t.RawID, AccessHash: t.AccessHash}
		ok := true
		for start := 0; start < len(t.IDs); start += batchSize {
			end := start + batchSize
			if end > len(t.IDs) {
				end = len(t.IDs)
			}
			batch := t.IDs[start:end]
			if err := callWithFloodRetry(ctx, func() error {
				return m.deleteBatch(ctx, channel, batch)
			}); err != nil {
				failed = append(failed, fmt.Sprintf("«%s»: %v", t.Title, err))
				ok = false
				break
			}
			deleted += len(batch)
			time.Sleep(500 * time.Millisecond)
		}
		if ok {
			chatsDone++
			m.clearOpsecChat(t.RawID)
		}
	}
	msg := fmt.Sprintf("👻 Opsec: выжжено %d сообщений в %d чатах", deleted, chatsDone)
	if len(failed) > 0 {
		msg += "; ошибки: " + strings.Join(failed, "; ")
	}
	m.logMessage(msg)
}

// clearOpsecChat drops a purged chat from the stored scan so the next
// report reflects reality without a full rescan.
func (m *module) clearOpsecChat(rawID int64) {
	_ = m.withState(func(s *state) error {
		if s.Opsec == nil {
			return nil
		}
		kept := s.Opsec.Chats[:0]
		for _, c := range s.Opsec.Chats {
			if c.RawID != rawID {
				kept = append(kept, c)
			}
		}
		s.Opsec.Chats = kept
		return nil
	})
}

// runOpsec walks the global self-message index via messages.search with
// peer=inputPeerSelf, aggregating own-message counts per channel chat.
func (m *module) runOpsec() {
	defer m.opsecRunning.Store(false)
	ctx := context.Background()

	// A warm dialog cache marks which chats are still "ours"; failure here
	// must not block the scan itself.
	_ = m.syncDialogs(ctx)

	selfID := m.selfID.Load()
	agg := make(map[int64]*opsecChat)
	meta := make(map[int64]tg.ChatClass)
	limit := opsecPageLimit
	maxDate := 0
	seenIDs := make(map[int]bool)
	pages := 0
	var scanErr string

	for pages < opsecMaxPages {
		var body []byte
		req := &tg.MessagesSearchRequest{
			Peer:    &tg.InputPeerSelf{},
			Q:       "",
			Filter:  &tg.InputMessagesFilterEmpty{},
			MaxDate: maxDate,
			Limit:   limit,
		}
		req.SetFromID(&tg.InputPeerSelf{})
		err := callWithFloodRetry(ctx, func() error {
			var callErr error
			body, callErr = m.call.call(ctx, req)
			return callErr
		})
		if err != nil {
			var rpcErr *TelegramRPCError
			if errors.As(err, &rpcErr) && rpcErr.Kind == "internal" && limit > 1 {
				if limit > 5 {
					limit = 5
				} else {
					limit = 1
				}
				continue
			}
			scanErr = err.Error()
			break
		}
		messages, users, chats, err := decodeSearchPage(body)
		if err != nil {
			scanErr = err.Error()
			break
		}
		for _, chat := range chats {
			if channel, ok := chat.(*tg.Channel); ok {
				meta[channel.GetID()] = channel
			}
		}
		if selfID == 0 {
			selfID = findSelfID(users)
			if selfID != 0 {
				m.selfID.Store(selfID)
			}
		}
		if selfID == 0 {
			id, idErr := m.resolveSelfID(ctx)
			if idErr != nil {
				scanErr = idErr.Error()
				break
			}
			selfID = id
		}
		if len(messages) == 0 {
			break
		}
		pageOldest := 0
		newOnPage := 0
		for _, message := range messages {
			concrete, ok := message.(*tg.Message)
			if !ok {
				continue
			}
			id := concrete.GetID()
			if seenIDs[id] {
				continue
			}
			from, has := concrete.GetFromID()
			peerUser, isUser := from.(*tg.PeerUser)
			if !has || !isUser || peerUser.UserID != selfID {
				continue
			}
			peerChannel, isChannel := concrete.GetPeerID().(*tg.PeerChannel)
			if !isChannel {
				continue
			}
			seenIDs[id] = true
			newOnPage++
			date := int64(concrete.GetDate())
			entry := agg[peerChannel.ChannelID]
			if entry == nil {
				entry = &opsecChat{RawID: peerChannel.ChannelID}
				agg[peerChannel.ChannelID] = entry
			}
			entry.Count++
			if date > entry.Newest {
				entry.Newest = date
				entry.NewestMsg = id
			}
			if entry.Oldest == 0 || date < entry.Oldest {
				entry.Oldest = date
			}
			if len(entry.IDs) < opsecMaxIDs {
				entry.IDs = append(entry.IDs, id)
			}
			if pageOldest == 0 || date < int64(pageOldest) {
				pageOldest = int(date)
			}
		}
		pages++
		if pages%25 == 0 {
			fmt.Fprintf(os.Stderr, "opsec scan progress: pages=%d chats=%d seen=%d maxDate=%d\n", pages, len(agg), len(seenIDs), maxDate)
		}
		if pages%200 == 0 {
			m.saveOpsecPartial(agg, meta, m.dialogSet())
		}
		if newOnPage == 0 && pageOldest == 0 {
			break
		}
		if pageOldest == 0 || (maxDate != 0 && pageOldest >= maxDate) {
			break
		}
		maxDate = pageOldest - 1
		if len(messages) < limit {
			break
		}
	}

	dialogSet := m.dialogSet()
	var chatsOut []opsecChat
	for rawID, entry := range agg {
		if channel, ok := meta[rawID].(*tg.Channel); ok {
			entry.Title = channel.GetTitle()
			if hash, has := channel.GetAccessHash(); has {
				entry.AccessHash = hash
			}
		}
		entry.InDialog = dialogSet[rawID]
		chatsOut = append(chatsOut, *entry)
	}
	sort.Slice(chatsOut, func(i, j int) bool { return chatsOut[i].Count > chatsOut[j].Count })
	if scanErr == "" || len(chatsOut) > 0 {
		_ = m.withState(func(s *state) error {
			if s.Opsec == nil || s.Opsec.ScannedAt == 0 {
				s.Opsec = &opsecState{}
			}
			if len(chatsOut) > 0 {
				s.Opsec.Chats = chatsOut
				s.Opsec.ScannedAt = time.Now().Unix()
			}
			return nil
		})
	}
	if scanErr != "" {
		fmt.Fprintf(os.Stderr, "opsec scan: %s\n", scanErr)
	}
}

// resolveSelfID fetches the account's own user id via
// users.getFullUser(inputPeerSelf): global search responses do not carry a
// self-flagged user in their users vector, unlike history pages.
func (m *module) resolveSelfID(ctx context.Context) (int64, error) {
	if id := m.selfID.Load(); id != 0 {
		return id, nil
	}
	var body []byte
	err := callWithFloodRetry(ctx, func() error {
		var callErr error
		body, callErr = m.call.call(ctx, &tg.UsersGetFullUserRequest{ID: &tg.InputUserSelf{}})
		return callErr
	})
	if err != nil {
		return 0, err
	}
	if len(body) < 4 || binary.LittleEndian.Uint32(body[:4]) != tg.UsersUserFullTypeID {
		return 0, fmt.Errorf("unexpected getFullUser constructor")
	}
	var full tg.UsersUserFull
	if err := full.DecodeBare(buffer(body[4:])); err != nil {
		return 0, fmt.Errorf("decode userFull: %w", err)
	}
	id := findSelfID(full.Users)
	if id == 0 {
		return 0, fmt.Errorf("self flag missing in userFull")
	}
	m.selfID.Store(id)
	return id, nil
}

func (m *module) dialogSet() map[int64]bool {
	set := make(map[int64]bool)
	m.peekState(func(s *state) {
		for _, d := range s.Discovered {
			set[d.ID] = true
		}
	})
	return set
}

// saveOpsecPartial persists an in-progress aggregation so a restart does
// not discard a long scan; reports treat it as scan results.
func (m *module) saveOpsecPartial(agg map[int64]*opsecChat, meta map[int64]tg.ChatClass, dialogs map[int64]bool) {
	var chatsOut []opsecChat
	for rawID, entry := range agg {
		e := *entry
		if channel, ok := meta[rawID].(*tg.Channel); ok {
			e.Title = channel.GetTitle()
			if hash, has := channel.GetAccessHash(); has {
				e.AccessHash = hash
			}
		}
		e.InDialog = dialogs[rawID]
		chatsOut = append(chatsOut, e)
	}
	sort.Slice(chatsOut, func(i, j int) bool { return chatsOut[i].Count > chatsOut[j].Count })
	_ = m.withState(func(s *state) error {
		if s.Opsec == nil {
			s.Opsec = &opsecState{}
		}
		s.Opsec.Chats = chatsOut
		s.Opsec.ScannedAt = time.Now().Unix()
		return nil
	})
}
