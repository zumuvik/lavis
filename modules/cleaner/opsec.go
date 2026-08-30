package main

import (
	"context"
	"encoding/binary"
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
		ids := t.IDs
		if len(ids) == 0 {
			ids = m.collectOpsecIDs(ctx, t)
		}
		ok := true
		for start := 0; start < len(ids); start += batchSize {
			end := start + batchSize
			if end > len(ids) {
				end = len(ids)
			}
			batch := ids[start:end]
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
// runOpsec aggregates the account's own message footprint per dialog chat.
// The server does not support author-only global search, so every cached
// dialog gets one messages.search with a concrete self from_id; the
// returned total is the own-message count and the first hit the newest
// message. Chats flagged left are ghosts (dialog kept, membership gone).
func (m *module) runOpsec() {
	defer m.opsecRunning.Store(false)
	ctx := context.Background()

	if err := m.syncDialogs(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "opsec dialogs: %v\n", err)
	}
	if _, err := m.resolveSelfID(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "opsec self resolve: %v\n", err)
		return
	}
	selfPeer := m.selfPeer()

	var dialogs []groupEntry
	m.peekState(func(s *state) {
		dialogs = append([]groupEntry(nil), s.Discovered...)
	})

	var chatsOut []opsecChat
	probe := 0
	for _, d := range dialogs {
		if d.AccessHash == 0 {
			continue
		}
		req := &tg.MessagesSearchRequest{
			Peer:    &tg.InputPeerChannel{ChannelID: d.ID, AccessHash: d.AccessHash},
			Q:       "",
			Filter:  &tg.InputMessagesFilterEmpty{},
			MaxDate: 0,
			Limit:   opsecPageLimit,
		}
		req.SetFromID(selfPeer)
		var body []byte
		err := callWithFloodRetry(ctx, func() error {
			var callErr error
			body, callErr = m.call.call(ctx, req)
			return callErr
		})
		if err != nil {
			if probe < 5 {
				fmt.Fprintf(os.Stderr, "opsec probe %q: %v\n", d.Title, err)
			}
			continue
		}
		count, newestMsg, newestDate := searchFootprint(body)
		if probe < 5 {
			fmt.Fprintf(os.Stderr, "opsec probe %q: total=%d newestMsg=%d\n", d.Title, count, newestMsg)
		}
		probe++
		if count == 0 {
			continue
		}
		chatsOut = append(chatsOut, opsecChat{
			RawID:      d.ID,
			AccessHash: d.AccessHash,
			Title:      d.Title,
			Count:      count,
			Newest:     newestDate,
			NewestMsg:  newestMsg,
			InDialog:   !d.Left,
			IDs:        nil,
		})
		if probe%25 == 0 {
			m.saveOpsecList(chatsOut)
		}
	}
	m.saveOpsecList(chatsOut)
	if scanComplete := true; !scanComplete {
		_ = scanComplete
	}
}

// searchFootprint extracts own-message total and newest message from a
// per-chat search page.
func searchFootprint(body []byte) (int, int, int64) {
	value, err := tg.DecodeMessagesMessages(buffer(body))
	if err != nil {
		return 0, 0, 0
	}
	total := 0
	var messages []tg.MessageClass
	switch m := value.(type) {
	case *tg.MessagesChannelMessages:
		total = m.GetCount()
		messages = m.GetMessages()
	case *tg.MessagesMessagesSlice:
		total = m.GetCount()
		messages = m.GetMessages()
	case *tg.MessagesMessages:
		messages = m.GetMessages()
		total = len(messages)
	}
	newestMsg, newestDate := 0, int64(0)
	for _, message := range messages {
		concrete, ok := message.(*tg.Message)
		if !ok {
			continue
		}
		if id := concrete.GetID(); id > newestMsg {
			newestMsg = id
			newestDate = int64(concrete.GetDate())
		}
	}
	return total, newestMsg, newestDate
}

// saveOpsecList persists the current scan result snapshot.
func (m *module) saveOpsecList(chats []opsecChat) {
	sort.Slice(chats, func(i, j int) bool { return chats[i].Count > chats[j].Count })
	_ = m.withState(func(s *state) error {
		if s.Opsec == nil {
			s.Opsec = &opsecState{}
		}
		s.Opsec.Chats = chats
		s.Opsec.ScannedAt = time.Now().Unix()
		return nil
	})
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
func firstCtor(body []byte) uint32 {
	if len(body) < 4 {
		return 0
	}
	return binary.LittleEndian.Uint32(body[:4])
}

// resolveSelfID fetches the account's own user id and access hash via
// users.getFullUser(inputPeerSelf).
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
	if firstCtor(body) != tg.UsersUserFullTypeID {
		return 0, fmt.Errorf("unexpected getFullUser constructor %x", firstCtor(body))
	}
	var full tg.UsersUserFull
	if err := full.DecodeBare(buffer(body[4:])); err != nil {
		return 0, fmt.Errorf("decode userFull: %w", err)
	}
	id := findSelfID(full.Users)
	if id == 0 {
		return 0, fmt.Errorf("self flag missing in userFull")
	}
	for _, user := range full.Users {
		concrete, ok := user.(*tg.User)
		if !ok || !concrete.GetSelf() {
			continue
		}
		if hash, has := concrete.GetAccessHash(); has {
			m.selfHash.Store(hash)
		}
	}
	m.selfID.Store(id)
	return id, nil
}

// selfPeer returns a concrete InputPeerUser for the account: the server
// ignores from_id=inputPeerSelf in messages.search but honours the real
// user peer.
func (m *module) selfPeer() tg.InputPeerClass {
	if id, hash := m.selfID.Load(), m.selfHash.Load(); id != 0 && hash != 0 {
		return &tg.InputPeerUser{UserID: id, AccessHash: hash}
	}
	return &tg.InputPeerSelf{}
}

// collectOpsecIDs lazily walks the per-chat self search to gather message
// ids for a purge, capped so one target cannot block forever.
func (m *module) collectOpsecIDs(ctx context.Context, c opsecChat) []int {
	peer := &tg.InputPeerChannel{ChannelID: c.RawID, AccessHash: c.AccessHash}
	var ids []int
	seen := make(map[int]bool)
	limit := opsecPageLimit
	maxDate := 0
	for page := 0; page < opsecMaxPages && len(ids) < opsecMaxIDs; page++ {
		req := &tg.MessagesSearchRequest{
			Peer:    peer,
			Q:       "",
			Filter:  &tg.InputMessagesFilterEmpty{},
			MaxDate: maxDate,
			Limit:   limit,
		}
		req.SetFromID(m.selfPeer())
		var body []byte
		err := callWithFloodRetry(ctx, func() error {
			var callErr error
			body, callErr = m.call.call(ctx, req)
			return callErr
		})
		if err != nil {
			break
		}
		messages, _, _, err := decodeSearchPage(body)
		if err != nil || len(messages) == 0 {
			break
		}
		oldest := 0
		added := 0
		self := m.selfID.Load()
		for _, message := range messages {
			concrete, ok := message.(*tg.Message)
			if !ok {
				continue
			}
			from, has := concrete.GetFromID()
			peerUser, isUser := from.(*tg.PeerUser)
			if !has || !isUser || peerUser.UserID != self {
				continue
			}
			id := concrete.GetID()
			if !seen[id] {
				seen[id] = true
				ids = append(ids, id)
				added++
			}
			if d := concrete.GetDate(); oldest == 0 || d < oldest {
				oldest = d
			}
		}
		if oldest == 0 || (maxDate != 0 && oldest >= maxDate) || added == 0 && len(messages) < limit {
			break
		}
		maxDate = oldest - 1
		if len(messages) < limit {
			break
		}
	}
	return ids
}
