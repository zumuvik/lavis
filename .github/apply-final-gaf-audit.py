from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match in {path}, got {count}")
    file.write_text(text.replace(old, new, 1))


def replace_between(path: str, start: str, end: str, replacement: str) -> None:
    file = Path(path)
    text = file.read_text()
    begin = text.index(start)
    finish = text.index(end, begin)
    file.write_text(text[:begin] + replacement + text[finish:])


replace_once(
    "modules/gaf/main.go",
    'Revision  uint64     `json:"revision"`',
    'Revision  uint64     `json:"-"`',
)

replace_between(
    "modules/gaf/main.go",
    "func (m *module) toggle(arguments string) (string, error) {",
    "func (m *module) handleEvent(event string, payload eventPayload) []eventAction {",
    '''func (m *module) toggle(arguments string) (string, error) {
\targuments = strings.TrimSpace(arguments)
\tif arguments == "" || isSwitch(arguments) {
\t\tvalue := !m.state.Enabled
\t\tif arguments != "" {
\t\t\tvalue = switchValue(arguments, value)
\t\t}
\t\tm.state.Enabled = value
\t\tif err := m.save(); err != nil {
\t\t\treturn "", err
\t\t}
\t\treturn fmt.Sprintf("GAF: %s", onOff(value)), nil
\t}

\tkey := arguments
\trequestedSwitch := ""
\tindex := m.findTrigger(key)
\tif index < 0 {
\t\tfields := strings.Fields(arguments)
\t\tif len(fields) > 1 && isSwitch(fields[len(fields)-1]) {
\t\t\trequestedSwitch = fields[len(fields)-1]
\t\t\tkey = strings.TrimSpace(strings.TrimSuffix(arguments, requestedSwitch))
\t\t\tindex = m.findTrigger(key)
\t\t}
\t}
\tif key == "" {
\t\treturn "", errors.New("использование: toggle <номер|триггер> [on|off]")
\t}
\tif index < 0 {
\t\treturn "", errors.New("триггер не найден")
\t}
\tvalue := !m.state.Triggers[index].Enabled
\tif requestedSwitch != "" {
\t\tvalue = switchValue(requestedSwitch, value)
\t}
\tm.state.Triggers[index].Enabled = value
\tif err := m.save(); err != nil {
\t\treturn "", err
\t}
\treturn fmt.Sprintf("Триггер «%s»: %s", m.state.Triggers[index].Word, onOff(value)), nil
}

''',
)

replace_between(
    "modules/gaf/main.go",
    "func (m *module) handleEvent(event string, payload eventPayload) []eventAction {",
    "func (m *module) reactionsFor(text string) []reaction {",
    '''func (m *module) handleEvent(event string, payload eventPayload) []eventAction {
\tif !m.state.Enabled || payload.MessageRef == "" || payload.MessageKey == "" {
\t\treturn nil
\t}
\trevision, _ := strconv.ParseUint(payload.EventID, 10, 64)
\tprevious, known := m.state.Active[payload.MessageKey]
\tif known && revision != 0 && previous.Revision != 0 && revision <= previous.Revision {
\t\treturn nil
\t}

\tdesired := m.reactionsFor(payload.Text)
\tnow := time.Now().Unix()
\tentry := activeEntry{Reactions: desired, Revision: revision, SeenAt: now}
\tif len(desired) == 0 {
\t\tif event == "message.created" && !known {
\t\t\treturn nil
\t\t}
\t\tshouldRemove := event == "message.edited" && known && len(previous.Reactions) > 0
\t\tm.state.Active[payload.MessageKey] = entry
\t\tm.pruneActive()
\t\tm.saveEventState()
\t\tif !shouldRemove {
\t\t\treturn nil
\t\t}
\t\treturn []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: []reaction{}}}
\t}
\tif known && equalReactions(previous.Reactions, desired) {
\t\tm.state.Active[payload.MessageKey] = entry
\t\tm.saveEventState()
\t\treturn nil
\t}
\tm.state.Active[payload.MessageKey] = entry
\tm.pruneActive()
\tm.saveEventState()
\treturn []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: desired}}
}

func (m *module) saveEventState() {
\tif err := m.save(); err != nil {
\t\tfmt.Fprintln(os.Stderr, err)
\t}
}

''',
)

replace_once(
    "modules/gaf/main.go",
    '''\t\t} else if id, ok := customID(item.Text); ok {
\t\t\tvalue = reaction{Type: "custom_emoji", DocumentID: id}
\t\t} else {''',
    '''\t\t} else if hasCustomIDPrefix(item.Text) {
\t\t\tid, ok := customID(item.Text)
\t\t\tif !ok {
\t\t\t\treturn nil, errors.New("некорректный Premium emoji document_id")
\t\t\t}
\t\t\tvalue = reaction{Type: "custom_emoji", DocumentID: id}
\t\t} else {''',
)

replace_once(
    "modules/gaf/main.go",
    '''func customID(value string) (string, bool) {
\tlower := strings.ToLower(value)''',
    '''func hasCustomIDPrefix(value string) bool {
\tlower := strings.ToLower(value)
\treturn strings.HasPrefix(lower, "ce:") || strings.HasPrefix(lower, "custom:")
}

func customID(value string) (string, bool) {
\tlower := strings.ToLower(value)''',
)

replace_once(
    "modules/gaf/main_test.go",
    '''import (
\t"path/filepath"
\t"testing"
\t"unicode/utf16"
)''',
    '''import (
\t"encoding/json"
\t"path/filepath"
\t"strings"
\t"testing"
\t"unicode/utf16"
)''',
)

with Path("modules/gaf/main_test.go").open("a") as file:
    file.write(r'''

func TestToggleSupportsMultiwordTrigger(t *testing.T) {
\tm := module{
\t\tpath: filepath.Join(t.TempDir(), "state.json"),
\t\tstate: state{
\t\t\tEnabled: true,
\t\t\tNextID:  2,
\t\t\tTriggers: []trigger{{
\t\t\t\tID: 1, Word: "очень никс", Enabled: true,
\t\t\t\tReactions: []reaction{{Type: "emoji", Emoji: "👍"}},
\t\t\t}},
\t\t\tActive: map[string]activeEntry{},
\t\t},
\t}
\tif _, err := m.toggle("очень никс off"); err != nil {
\t\tt.Fatal(err)
\t}
\tif m.state.Triggers[0].Enabled {
\t\tt.Fatal("multiword trigger should be disabled")
\t}
}

func TestToggleSupportsTriggerEndingInSwitchWord(t *testing.T) {
\tm := module{
\t\tpath: filepath.Join(t.TempDir(), "state.json"),
\t\tstate: state{
\t\t\tEnabled: true,
\t\t\tNextID:  2,
\t\t\tTriggers: []trigger{{
\t\t\t\tID: 1, Word: "turn off", Enabled: true,
\t\t\t\tReactions: []reaction{{Type: "emoji", Emoji: "👍"}},
\t\t\t}},
\t\t\tActive: map[string]activeEntry{},
\t\t},
\t}
\tif _, err := m.toggle("turn off"); err != nil {
\t\tt.Fatal(err)
\t}
\tif m.state.Triggers[0].Enabled {
\t\tt.Fatal("exact trigger name must win over switch parsing")
\t}
}

func TestRuntimeRevisionIsNotPersisted(t *testing.T) {
\tm := testModule(t)
\tm.state.Active["stable"] = activeEntry{
\t\tReactions: []reaction{{Type: "emoji", Emoji: "👍"}},
\t\tRevision:  999,
\t\tSeenAt:    1,
\t}
\tdata, err := json.Marshal(m.state)
\tif err != nil {
\t\tt.Fatal(err)
\t}
\tif strings.Contains(string(data), `"revision"`) {
\t\tt.Fatalf("runtime revision leaked into state: %s", data)
\t}
\tvar restored state
\tif err := json.Unmarshal(data, &restored); err != nil {
\t\tt.Fatal(err)
\t}
\trestarted := module{path: filepath.Join(t.TempDir(), "state.json"), state: restored}
\tactions := restarted.handleEvent("message.edited", eventPayload{
\t\tEventID: "1", MessageRef: "edited", MessageKey: "stable", Text: "без триггера",
\t})
\tif len(actions) != 1 || actions[0].Reactions == nil || len(actions[0].Reactions) != 0 {
\t\tt.Fatalf("restart must accept the new edit and remove reactions: %#v", actions)
\t}
}

func TestCreatedNonMatchDoesNotPopulateActiveState(t *testing.T) {
\tm := testModule(t)
\tactions := m.handleEvent("message.created", eventPayload{
\t\tEventID: "10", MessageRef: "created", MessageKey: "no-match", Text: "обычное сообщение",
\t})
\tif len(actions) != 0 || len(m.state.Active) != 0 {
\t\tt.Fatalf("nonmatching created event should be ignored: actions=%#v active=%#v", actions, m.state.Active)
\t}
}

func TestMalformedCustomIDIsRejected(t *testing.T) {
\tif _, err := parseReactions([]token{{Text: "ce:not-a-number"}}, nil); err == nil {
\t\tt.Fatal("malformed diagnostic custom emoji ID must be rejected")
\t}
}
''')

replace_once(
    "src/updates.rs",
    '''    if should_prepare_message_event(event_protected) {
        let event = if edited {
            crate::external_modules::protocol::MessageEventKind::Edited
        } else {
            crate::external_modules::protocol::MessageEventKind::Created
        };
        let entities = crate::external_modules::entities::project_custom_emoji_entities(
            message.fmt_entities(),
            0,
            message.text().encode_utf16().count(),
        );''',
    '''    // New command/setup messages stay private. If an already-projected message is
    // edited into protected content, emit a redacted edit so modules can reconcile
    // prior actions without receiving command or setup text.
    if should_prepare_message_event(edited, event_protected) {
        let event = if edited {
            crate::external_modules::protocol::MessageEventKind::Edited
        } else {
            crate::external_modules::protocol::MessageEventKind::Created
        };
        let event_text = if event_protected { "" } else { message.text() };
        let entities = if event_protected {
            Vec::new()
        } else {
            crate::external_modules::entities::project_custom_emoji_entities(
                message.fmt_entities(),
                0,
                message.text().encode_utf16().count(),
            )
        };''',
)

replace_once(
    "src/updates.rs",
    '''            event,
            message.text(),
            outgoing,
            entities,''',
    '''            event,
            event_text,
            outgoing,
            entities,''',
)

replace_once(
    "src/updates.rs",
    '''fn should_prepare_message_event(event_protected: bool) -> bool {
    !event_protected
}''',
    '''fn should_prepare_message_event(edited: bool, event_protected: bool) -> bool {
    edited || !event_protected
}''',
)

replace_once(
    "src/updates.rs",
    '''        assert!(!should_prepare_message_event(true));
        assert!(should_prepare_message_event(false));''',
    '''        assert!(!should_prepare_message_event(false, true));
        assert!(should_prepare_message_event(true, true));
        assert!(should_prepare_message_event(false, false));
        assert!(should_prepare_message_event(true, false));''',
)

replace_once(
    "docs/module-api-v4.md",
    '''`message_ref` is valid only for the current request and must be copied into an
action. `message_key` is stable for the same Telegram message and module across
`message.created` and `message.edited`; modules may use it as a reconciliation
key. Neither value exposes the Telegram peer or message ID.
''',
    '''`message_ref` is valid only for the current request and must be copied into an
action. `message_key` is stable for the same Telegram message and module across
`message.created` and `message.edited`; modules may use it as a reconciliation
key. Neither value exposes the Telegram peer or message ID.

New Lavis command/setup messages are not projected to modules. If a previously
projected message is edited into protected command/setup content, v4 subscribers
receive a redacted `message.edited` event with empty `text` and `entities`. This
allows modules to remove prior actions without receiving the protected text.
''',
)

replace_once(
    "src/credentials.rs",
    '''    use std::{
        collections::HashMap,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn path() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "lavis-credentials-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));''',
    '''    use std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

    fn path() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "lavis-credentials-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ));''',
)

Path("modules/gaf/build-lmod.sh").write_text('''#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
rm -rf build
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/gaf .
chmod 700 build/gaf
cp module.json build/module.json
chmod 600 build/module.json
TZ=UTC touch -t 198001010000 build/gaf build/module.json
rm -f dist/gaf.lmod
(
  cd build
  zip -q -0 -X ../dist/gaf.lmod module.json gaf
)
printf 'built %s\\n' "$(realpath dist/gaf.lmod)"
''')

replace_once(
    "flake.nix",
    '''          fastfetch
          python3 # JSON-line external-process fixtures
''',
    '''          fastfetch
          python3 # JSON-line external-process fixtures
          go
          zip
''',
)

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Check formatting
        run: nix develop -c cargo fmt --check
''',
    '''      - name: Test GAF module and artifact
        run: |
          cp modules/gaf/dist/gaf.lmod /tmp/gaf.lmod
          nix develop -c bash -c 'set -euo pipefail; test -z "$(gofmt -l modules/gaf/*.go)"; cd modules/gaf; go test ./...; go vet ./...; ./build-lmod.sh'
          cmp /tmp/gaf.lmod modules/gaf/dist/gaf.lmod
          python3 - <<'PY'
          import zipfile
          with zipfile.ZipFile("modules/gaf/dist/gaf.lmod") as archive:
              assert archive.namelist() == ["module.json", "gaf"]
              assert all(item.compress_type == zipfile.ZIP_STORED for item in archive.infolist())
              modes = {item.filename: (item.external_attr >> 16) & 0o777 for item in archive.infolist()}
              assert modes == {"module.json": 0o600, "gaf": 0o700}, modes
          PY

      - name: Check formatting
        run: nix develop -c cargo fmt --check
''',
)
