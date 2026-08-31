# Участие в разработке Lavis

Lavis — статически типизированный Telegram userbot на Rust и Nix. Вносите минимальные связные изменения, добавляйте тесты для изменённого поведения и обновляйте пользовательскую документацию при изменении интерфейса, команд, безопасности, manifest/protocol или конфигурации.

Основные контракты:

- [Module API v1](docs/module-api-v1.md) — встроенный compile-time registry;
- [Module API v2/v3](docs/module-api-v2.md) — внешние manifests и JSON-line protocol;
- [Module API v4](docs/module-api-v4.md) — редактирование сообщений и реакции;
- [Module API v5](docs/module-api-v5.md) — gateway статуса аккаунта;
- [Module API v6](docs/module-api-v6.md) — capability-gated raw MTProto;
- [External Modules](docs/external-modules.md) — lifecycle внешних модулей;
- [Packaging `.lmod`](docs/lmod-packaging.md) — installer package boundary.

## Built-in Module API v1

Используйте утверждённые публичные типы: `ModuleId`, `ModuleOrigin`, `ModuleCapability`, `ModuleSpec`, `CommandKind`, `CommandRisk`, `CommandDefinition` и `Action`.

- `ModuleOrigin::External` в v1 — только проверяемые метаданные для рендеринга; external runtime не регистрируется в статических таблицах.
- `CommandRisk`: `ReadOnly`, `PersistentStateChange`, `RestrictedProcess`, `ArbitraryProcess`, `Privileged`, `ExternalCodeInstall`.
- `setup` использует `Privileged`, `lm` — `ExternalCodeInstall`, `fastfetch` — `RestrictedProcess`.
- `CommandDefinition`: `kind`, `name`, `usage`, `summary_ru`, `description_ru`, `examples`, `risk`, `icon`, `aliasable`, `module`.

Публичные запросы статического реестра: `modules()`, `module_by_id`, `module_by_name`, `commands()`, `command_by_kind`, `command_by_name`, `commands_for_module`, `module_for_command`. Не обходите их дублирующими каталогами.

### Регистрация встроенной команды

1. Добавьте/обновите `ModuleSpec`.
2. Добавьте `CommandKind` и полный `CommandDefinition`.
3. Добавьте типизированный вариант `Action`.
4. Добавьте соответствие в `dispatch`.
5. Добавьте отдельный parser аргументов.
6. Добавьте runtime behavior.
7. Добавьте тесты реестра, parser, dispatch, runtime и Help v2.
8. Обновите README и соответствующие документы.

`examples` всегда без префикса: `ping`, `help system`, `lm list`. UI сам подставляет активный префикс.

## External module subsystem

Разделяйте обязанности:

- Telegram acquisition только получает bounded document bytes;
- inspection проверяет untrusted archive и создаёт owned staging + review plan;
- approval store является единственным owner pending inspections;
- installer выполняет только filesystem transaction;
- manager/runtime регистрирует descriptor, управляет process state и snapshots;
- enabled state хранится отдельно и не меняется установкой.

Не создавайте параллельные token/staging maps или повторную post-install validation в runtime.

### Manifest и protocol changes

При изменении schema/protocol:

- сохраняйте `serde(deny_unknown_fields)` для manifest boundaries;
- обновляйте validator, protocol serializer/parser, manager/process tests и docs;
- schema 2 не должна молча принимать schema 3-only fields;
- новые subscriptions/actions должны иметь явные capabilities и scoped validation;
- входные/выходные строки, IDs, entities и actions должны иметь bounded limits;
- request ID обязан точно связывать request и reply.

### `.lmod` acquisition and inspection

Сохраняйте fail-closed invariants:

- state-changing `lm install/confirm/cancel` только из нового собственного сообщения в Saved Messages;
- только same-message `Media::Document` с lowercase `.lmod` suffix;
- declared size — preflight, actual streamed bytes — обязательная boundary;
- download прекращается сразу после превышения лимита;
- ZIP принимает только unencrypted stored entries;
- root `module.json` ровно один;
- no symlink/device/FIFO/path traversal/special bits;
- staging root и wrappers private, marker schema строгая;
- cleanup не следует symlink и не трогает module root.

Тесты acquisition должны покрывать missing declared size, early declared rejection, exact limit, actual oversize early abort и transport error. Inspection regression suite должен покрывать paths, entry types, archive limits, manifest/schema/capabilities, staging ownership и deterministic fingerprint.

### Approval and installation

- ApprovalId — полный canonical 80-bit Crockford Base32 `XXXX-XXXX-XXXX-XXXX`; prefix matching запрещён.
- TTL — 600 секунд, expiry при `now >= expires_at`.
- approval одноразовый; redeem передаёт ownership staging installer'у.
- quota accounting освобождается при redeem/revoke/expiry/shutdown даже при cleanup failure.
- no-overwrite boundary — Linux `renameat2(..., RENAME_NOREPLACE)` через safe Rust API.
- `EEXIST` не перезаписывает target; `EXDEV` не имеет copy fallback.
- manifest валидируется один раз после rename из final target; validation failure вызывает rollback.
- ошибка удаления пустого wrapper после успешного commit не отменяет установку.
- installation не включает и не запускает модуль.
- stale/duplicate descriptor conflict не должен возвращать ложный success.

## Help v2

Help получает built-in metadata из v1 registry, а active external metadata — из runtime snapshot. `lm` использует специализированный renderer с Saved Messages flow, ApprovalId, TTL, disabled-after-install и no-sandbox warning.

Порядок разрешения темы: built-in canonical command → active external namespaced command → alias → built-in module → discovered external module → unknown.

`,lm list` не строится из active command refs: он показывает все discovered descriptors и manager status, включая disabled modules.

## Проверка безопасности

Перед PR проверьте:

- команда/событие доступно только предусмотренному авторизованному scope;
- входные данные разбираются строго;
- ошибки не раскрывают секреты, локальные пути, untrusted bytes или raw stderr;
- built-in process launch не использует `sh`, `bash`, `eval` или user-controlled program path;
- external installer не запускает код и не меняет enabled state;
- external runtime не заявляет sandbox, которой нет;
- новые файловые операции no-follow, bounded и не имеют TOCTOU `exists() + rename()` как security boundary;
- документация отражает фактический scope и ограничения.

## Секреты, профили и локальные данные

Никогда не добавляйте в Git, PR, тесты, документацию или логи API ID, API hash, пароли, коды входа, MTProto-сеанс, `credentials.json`, companion token, локальное состояние, профили или пользовательские абсолютные пути. В документации используйте символические XDG-пути.

## Ветки, PR и проверки

- Работайте в отдельной ветке с понятным названием.
- Один PR решает одну связанную задачу.
- В описании PR укажите поведение, security boundaries, тесты, обновлённые документы и фактически выполненные проверки.

Полная проверка:

```bash
nix develop
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
nix flake check
nix build
```
