from pathlib import Path
from textwrap import dedent, indent


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


def rust_block(value: str) -> str:
    return indent(dedent(value), "    ")


replace_once(
    "modules/gaf/main.go",
    'Revision  uint64     `json:"revision"`',
    'Revision  uint64     `json:"-"`',
)

replace_between(
    "modules/gaf/main.go",
    "func (m *module) toggle(arguments string) (string, error) {",
    "func (m *module) handleEvent(event string, payload eventPayload) []eventAction {",
    dedent(
        """\
        func (m *module) toggle(arguments string) (string, error) {
            arguments = strings.TrimSpace(arguments)
            if arguments == "" || isSwitch(arguments) {
                value := !m.state.Enabled
                if arguments != "" {
                    value = switchValue(arguments, value)
                }
                m.state.Enabled = value
                if err := m.save(); err != nil {
                    return "", err
                }
                return fmt.Sprintf("GAF: %s", onOff(value)), nil
            }

            key := arguments
            requestedSwitch := ""
            fields := strings.Fields(arguments)
            if len(fields) > 1 && isSwitch(fields[len(fields)-1]) {
                requestedSwitch = fields[len(fields)-1]
                suffixStart := strings.LastIndex(arguments, requestedSwitch)
                key = strings.TrimSpace(arguments[:suffixStart])
            }
            if key == "" {
                return "", errors.New("использование: toggle <номер|триггер> [on|off]")
            }
            index := m.findTrigger(key)
            if index < 0 {
                return "", errors.New("триггер не найден")
            }
            value := !m.state.Triggers[index].Enabled
            if requestedSwitch != "" {
                value = switchValue(requestedSwitch, value)
            }
            m.state.Triggers[index].Enabled = value
            if err := m.save(); err != nil {
                return "", err
            }
            return fmt.Sprintf("Триггер «%s»: %s", m.state.Triggers[index].Word, onOff(value)), nil
        }

        """
    ),
)

replace_between(
    "modules/gaf/main.go",
    "func (m *module) handleEvent(event string, payload eventPayload) []eventAction {",
    "func (m *module) reactionsFor(text string) []reaction {",
    dedent(
        """\
        func (m *module) handleEvent(event string, payload eventPayload) []eventAction {
            if !m.state.Enabled || payload.MessageRef == "" || payload.MessageKey == "" {
                return nil
            }
            revision, _ := strconv.ParseUint(payload.EventID, 10, 64)
            previous, known := m.state.Active[payload.MessageKey]
            if known && revision != 0 && previous.Revision != 0 && revision <= previous.Revision {
                return nil
            }

            desired := m.reactionsFor(payload.Text)
            now := time.Now().Unix()
            entry := activeEntry{Reactions: desired, Revision: revision, SeenAt: now}
            if len(desired) == 0 {
                if event == "message.created" && !known {
                    return nil
                }
                shouldRemove := event == "message.edited" && known && len(previous.Reactions) > 0
                m.state.Active[payload.MessageKey] = entry
                m.pruneActive()
                m.saveEventState()
                if !shouldRemove {
                    return nil
                }
                return []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: []reaction{}}}
            }
            if known && equalReactions(previous.Reactions, desired) {
                m.state.Active[payload.MessageKey] = entry
                m.saveEventState()
                return nil
            }
            m.state.Active[payload.MessageKey] = entry
            m.pruneActive()
            m.saveEventState()
            return []eventAction{{Type: "message.react", MessageRef: payload.MessageRef, Reactions: desired}}
        }

        func (m *module) saveEventState() {
            if err := m.save(); err != nil {
                fmt.Fprintln(os.Stderr, err)
            }
        }

        """
    ),
)

replace_once(
    "modules/gaf/main.go",
    '\t\t} else if id, ok := customID(item.Text); ok {\n\t\t\tvalue = reaction{Type: "custom_emoji", DocumentID: id}\n\t\t} else {',
    '\t\t} else if hasCustomIDPrefix(item.Text) {\n\t\t\tid, ok := customID(item.Text)\n\t\t\tif !ok {\n\t\t\t\treturn nil, errors.New("некорректный Premium emoji document_id")\n\t\t\t}\n\t\t\tvalue = reaction{Type: "custom_emoji", DocumentID: id}\n\t\t} else {',
)
replace_once(
    "modules/gaf/main.go",
    'func customID(value string) (string, bool) {\n\tlower := strings.ToLower(value)',
    'func hasCustomIDPrefix(value string) bool {\n\tlower := strings.ToLower(value)\n\treturn strings.HasPrefix(lower, "ce:") || strings.HasPrefix(lower, "custom:")\n}\n\nfunc customID(value string) (string, bool) {\n\tlower := strings.ToLower(value)',
)
replace_once(
    "modules/gaf/main.go",
    "• gaf toggle [on|off|номер|слово]",
    "• gaf toggle [on|off|номер|триггер]",
)

replace_once(
    "modules/gaf/main_test.go",
    'import (\n\t"path/filepath"\n\t"testing"\n\t"unicode/utf16"\n)',
    'import (\n\t"encoding/json"\n\t"path/filepath"\n\t"strings"\n\t"testing"\n\t"unicode/utf16"\n)',
)
test_file = Path("modules/gaf/main_test.go")
if "TestRuntimeRevisionIsNotPersisted" in test_file.read_text():
    raise SystemExit("audit tests already exist")
test_file.write_text(
    test_file.read_text()
    + dedent(
        """\

        func TestToggleSupportsMultiwordTrigger(t *testing.T) {
            m := module{
                path: filepath.Join(t.TempDir(), "state.json"),
                state: state{
                    Enabled: true,
                    NextID:  2,
                    Triggers: []trigger{{
                        ID: 1, Word: "очень никс", Enabled: true,
                        Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
                    }},
                    Active: map[string]activeEntry{},
                },
            }
            if _, err := m.toggle("очень никс off"); err != nil {
                t.Fatal(err)
            }
            if m.state.Triggers[0].Enabled {
                t.Fatal("multiword trigger should be disabled")
            }
        }

        func TestRuntimeRevisionIsNotPersisted(t *testing.T) {
            m := testModule(t)
            m.state.Active["stable"] = activeEntry{
                Reactions: []reaction{{Type: "emoji", Emoji: "👍"}},
                Revision:  999,
                SeenAt:    1,
            }
            data, err := json.Marshal(m.state)
            if err != nil {
                t.Fatal(err)
            }
            if strings.Contains(string(data), `"revision"`) {
                t.Fatalf("runtime revision leaked into state: %s", data)
            }
            var restored state
            if err := json.Unmarshal(data, &restored); err != nil {
                t.Fatal(err)
            }
            restarted := module{path: filepath.Join(t.TempDir(), "state.json"), state: restored}
            actions := restarted.handleEvent("message.edited", eventPayload{
                EventID: "1", MessageRef: "edited", MessageKey: "stable", Text: "без триггера",
            })
            if len(actions) != 1 || actions[0].Reactions == nil || len(actions[0].Reactions) != 0 {
                t.Fatalf("restart must accept the new edit and remove reactions: %#v", actions)
            }
        }

        func TestCreatedNonMatchDoesNotPopulateActiveState(t *testing.T) {
            m := testModule(t)
            actions := m.handleEvent("message.created", eventPayload{
                EventID: "10", MessageRef: "created", MessageKey: "no-match", Text: "обычное сообщение",
            })
            if len(actions) != 0 || len(m.state.Active) != 0 {
                t.Fatalf("nonmatching created event should be ignored: actions=%#v active=%#v", actions, m.state.Active)
            }
        }

        func TestMalformedCustomIDIsRejected(t *testing.T) {
            if _, err := parseReactions([]token{{Text: "ce:not-a-number"}}, nil); err == nil {
                t.Fatal("malformed diagnostic custom emoji ID must be rejected")
            }
        }
        """
    )
)

replace_once(
    "src/updates.rs",
    rust_block(
        """\
        if should_prepare_message_event(event_protected) {
            let event = if edited {
                crate::external_modules::protocol::MessageEventKind::Edited
            } else {
                crate::external_modules::protocol::MessageEventKind::Created
            };
            let entities = crate::external_modules::entities::project_custom_emoji_entities(
                message.fmt_entities(),
                0,
                message.text().encode_utf16().count(),
            );
        """
    ),
    rust_block(
        """\
        // New command/setup messages stay private. If an already-projected message is
        // edited into protected content, emit a redacted edit so modules can
        // reconcile prior actions without receiving the command text.
        if let Some(event_text) = message_event_text(edited, event_protected, message.text()) {
            let event = if edited {
                crate::external_modules::protocol::MessageEventKind::Edited
            } else {
                crate::external_modules::protocol::MessageEventKind::Created
            };
            let entities = if event_protected {
                Vec::new()
            } else {
                crate::external_modules::entities::project_custom_emoji_entities(
                    message.fmt_entities(),
                    0,
                    message.text().encode_utf16().count(),
                )
            };
        """
    ),
)
replace_once(
    "src/updates.rs",
    '            message.text(),\n            outgoing,\n            entities,',
    '            event_text,\n            outgoing,\n            entities,',
)
replace_once(
    "src/updates.rs",
    'fn should_prepare_message_event(event_protected: bool) -> bool {\n    !event_protected\n}',
    dedent(
        """\
        fn message_event_text(edited: bool, event_protected: bool, text: &str) -> Option<&str> {
            if !edited && event_protected {
                None
            } else if event_protected {
                Some("")
            } else {
                Some(text)
            }
        }
        """
    ).rstrip(),
)
replace_once(
    "src/updates.rs",
    "provision_completion_text, route, should_prepare_message_event,",
    "message_event_text, provision_completion_text, route,",
)
replace_once(
    "src/updates.rs",
    rust_block(
        """\
        #[test]
        fn protected_command_messages_are_not_projected_to_external_events() {
            assert!(!should_prepare_message_event(true));
            assert!(should_prepare_message_event(false));
        }
        """
    ),
    rust_block(
        """\
        #[test]
        fn protected_message_projection_is_redacted_only_for_edits() {
            assert_eq!(message_event_text(false, true, ",secret"), None);
            assert_eq!(message_event_text(true, true, ",secret"), Some(""));
            assert_eq!(message_event_text(false, false, "hello"), Some("hello"));
            assert_eq!(message_event_text(true, false, "edited"), Some("edited"));
        }
        """
    ),
)

replace_once(
    "docs/module-api-v4.md",
    "`message_key` is stable for the same Telegram message and module across\n`message.created` and `message.edited`; modules may use it as a reconciliation\nkey. Neither value exposes the Telegram peer or message ID.\n",
    "`message_key` is stable for the same Telegram message and module across\n`message.created` and `message.edited`; modules may use it as a reconciliation\nkey. Neither value exposes the Telegram peer or message ID.\n\nNew Lavis command/setup messages are not projected to modules. If a previously\nprojected message is edited into protected command/setup content, v4 subscribers\nreceive a redacted `message.edited` event with empty `text` and `entities`. This\nlets them remove prior actions without receiving the protected command text.\n",
)
replace_once(
    "modules/gaf/README.md",
    "- `,gaf toggle лайк [on|off]` — toggle one trigger.",
    "- `,gaf toggle очень никс [on|off]` — toggle one trigger, including a multi-word trigger.",
)

Path("modules/gaf/build-lmod.sh").write_text(
    dedent(
        """\
        #!/usr/bin/env bash
        set -euo pipefail
        cd "$(dirname "$0")"
        rm -rf build
        mkdir -p build dist
        CGO_ENABLED=0 GOOS=linux GOARCH=amd64 \\
          go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/gaf .
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
        """
    )
)

replace_once(
    "flake.nix",
    "          python3 # JSON-line external-process fixtures\n",
    "          python3 # JSON-line external-process fixtures\n          go\n          zip\n",
)

ci_step = indent(
    dedent(
        """\
        - name: Test GAF module
          run: |
            cp modules/gaf/dist/gaf.lmod /tmp/gaf.lmod
            nix develop -c bash -c 'set -euo pipefail; test -z "$(gofmt -l modules/gaf)"; cd modules/gaf; go test ./...; go vet ./...; ./build-lmod.sh'
            cmp /tmp/gaf.lmod modules/gaf/dist/gaf.lmod

        - name: Check formatting
          run: nix develop -c cargo fmt --check
        """
    ),
    "      ",
)
replace_once(
    ".github/workflows/ci.yml",
    "      - name: Check formatting\n        run: nix develop -c cargo fmt --check\n",
    ci_step,
)
replace_once(
    ".github/workflows/ci.yml",
    "      - name: Build package\n        run: nix build --print-build-logs\n",
    "      - name: Build package\n        run: nix build --print-build-logs\n\n      - name: Validate GAF package\n        run: ./result/bin/lavis modules validate modules/gaf/build/module.json\n",
)
