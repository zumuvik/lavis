<div align="center">

<img src="https://tangled.org/zumuvik.tngl.sh/lavis/raw/main/assets/logo.png" alt="Lavis logo" width="240">

# Lavis

**Быстрый и расширяемый Telegram userbot на Rust**

Работает напрямую через MTProto, ориентирован на Linux и предоставляет полноценную декларативную интеграцию с NixOS.

<p>
  <a href="https://tangled.org/zumuvik.tngl.sh/lavis/blob/main/README.md">
    <img src="https://img.shields.io/badge/README-English-5277C3.svg" alt="English README">
  </a>
  <a href="https://tangled.org/zumuvik.tngl.sh/lavis/pipelines">
    <img src="https://img.shields.io/badge/CI-Tangled-5A67D8.svg" alt="Tangled CI">
  </a>
  <a href="https://tangled.org/zumuvik.tngl.sh/lavis/blob/main/LICENSE">
    <img src="https://img.shields.io/badge/license-GPL--3.0--only-blue.svg" alt="GPL-3.0-only">
  </a>
  <img src="https://img.shields.io/badge/Rust-stable-orange.svg" alt="Rust">
  <img src="https://img.shields.io/badge/NixOS-supported-5277C3.svg" alt="NixOS">
  <img src="https://img.shields.io/badge/status-alpha-yellow.svg" alt="Alpha">
</p>

</div>

---

## Что такое Lavis

Lavis — персональный Telegram userbot, написанный на Rust с использованием библиотеки [grammers](https://github.com/Lonami/grammers).

Он работает через ваш Telegram-аккаунт, обрабатывает команды из собственных сообщений, редактирует исходное сообщение результатом и поддерживает как встроенные команды, так и внешние модули.

```text
,ping
,stats
,fastfetch
,help
```

Префикс по умолчанию — запятая `,`, но его можно изменить во время работы.

---

## Возможности

| Возможность | Описание |
| --- | --- |
| ⚡ **Нативное ядро на Rust** | Асинхронная работа с Telegram напрямую через MTProto |
| 🔐 **Локальная авторизация** | Telegram-сессия и API credentials хранятся локально с ограниченными правами |
| ✏️ **Редактирование сообщений** | Результат команды заменяет исходное исходящее сообщение |
| 🧩 **Внешние модули** | Модули на любом языке через JSON Lines protocol |
| 📦 **Формат `.lmod`** | Проверяемая установка модулей через «Сохранённые сообщения» |
| 🔌 **Module API v6** | Capability-based events, Telegram RPC adapters и ограниченный raw MTProto |
| 🐧 **NixOS integration** | Flake, пакет, dev shell и готовый NixOS-модуль |
| 🔧 **Префиксы и алиасы** | Сохраняемые алиасы и настраиваемый префикс |
| 🤖 **Companion bot** | Опциональная настройка companion-бота и рабочего пространства через BotFather |
| 🖥️ **Fastfetch** | Ограниченный и проверяемый вывод информации о системе |
| 🌐 **Русский / English UI** | Сохраняемый язык интерфейса и onboarding |

---

## Быстрый запуск

### 1. Получите Telegram API credentials

Создайте приложение на [my.telegram.org/apps](https://my.telegram.org/apps) и получите:

- `API ID`;
- `API hash`.

### 2. Запустите Lavis

```bash
nix run 'git+https://tangled.org/zumuvik.tngl.sh/lavis'
```

При первом интерактивном запуске Lavis сохраняет API credentials локально и запрашивает номер телефона, код входа и, если включена двухфакторная защита, пароль 2FA.

Авторизацию можно запустить отдельно:

```bash
nix run 'git+https://tangled.org/zumuvik.tngl.sh/lavis' -- auth
```

После успешной авторизации начните Telegram-введение из «Сохранённых сообщений»:

```text
,start
```

При необходимости сразу выберите язык:

```text
,start ru
,start en
```

> [!IMPORTANT]
> Не публикуйте API hash, `credentials.json`, базу Telegram-сессии, companion token, XDG-каталоги состояния или логи авторизации с секретами.

---

## Основные команды

| Команда | Назначение |
| --- | --- |
| `start` | Начать или продолжить onboarding |
| `language` | Показать или изменить язык интерфейса |
| `help` | Справка по командам, алиасам и модулям |
| `modules` | Встроенные и активные внешние модули |
| `ping` | Реальная задержка Telegram RPC |
| `stats` | Uptime, latency, процесс и информация о хосте |
| `prefix` | Показать, установить или сбросить префикс |
| `alias` | Управление сохраняемыми алиасами |
| `fastfetch` | Проверяемый вывод Fastfetch |
| `setup` | Настройка и восстановление companion-бота/workspace |
| `lm` | Установка и управление внешними `.lmod`-модулями |
| `reboot` | Перезапуск процесса Lavis |

<details>
<summary><b>Примеры команд</b></summary>

```text
,help
,help fastfetch
,modules
,ping
,stats
,prefix .
.alias add sys fastfetch --logo arch
.sys
.lm list
```

</details>

---

## Внешние модули

Внешний модуль Lavis — это отдельная исполняемая программа, которая общается с ядром через JSON Lines по `stdin` и `stdout`.

Модули можно писать на Rust, Go, Python или любом другом подходящем языке. Текущий Module API v6 поддерживает capability-gated Telegram-операции, события сообщений и ограниченный raw MTProto escape hatch.

Локальные операции с модулями:

```bash
lavis modules validate ./my-module/module.json
lavis modules enable my-module
lavis modules disable my-module
lavis modules status
```

### Установка `.lmod` через Telegram

1. Прикрепите `.lmod` к **новому собственному сообщению в «Сохранённых сообщениях»**.
2. Отправьте:

   ```text
   ,lm install
   ```

3. Проверьте план установки.
4. Подтвердите одноразовый Approval ID:

   ```text
   ,lm confirm XXXX-XXXX-XXXX-XXXX
   ```

5. Включите модуль и перезапустите Lavis:

   ```text
   ,lm enable <module-id>
   ,reboot
   ```

Полезные команды: `,lm list`, `,lm info <id>`, `,lm logs <id>` и `,lm doctor [id]`.

> [!WARNING]
> Внешние модули не помещаются в системную песочницу и работают с OS-правами пользователя Lavis. Capability checks ограничивают операции, проходящие через ядро, но не произвольный доступ модуля к ОС. Устанавливайте только доверенный код.

Подробнее: [External modules](docs/external-modules.md), [`.lmod` packaging](docs/lmod-packaging.md) и [Module API v6](docs/module-api-v6.md).

---

## Восстановление авторизации

Lavis блокирует локальную Telegram-сессию и различает восстанавливаемые проблемы авторизации и детерминированное повреждение базы сессии.

Полезные CLI-команды:

```bash
lavis credentials
lavis credentials reset
lavis auth doctor
lavis auth reset --backup
lavis logout
```

`auth doctor` проверяет локальную сессию. `auth reset --backup` заменяет сломанную сессию, сохраняя резервную копию. `logout` удаляет только локальную сессию и не отзывает Telegram-сеансы удалённо.

---

## NixOS

Добавьте Lavis в inputs вашего flake:

```nix
{
  inputs.lavis.url = "git+https://tangled.org/zumuvik.tngl.sh/lavis";
}
```

### Бинарный кеш

Lavis публикует Nix-сборки в Cachix. Добавьте кеш декларативно, чтобы Nix скачивал готовые сборки Lavis вместо локальной компиляции:

```nix
{
  nix.settings = {
    extra-substituters = [ "https://lavis.cachix.org" ];
    extra-trusted-public-keys = [
      "lavis.cachix.org-1:EXJoSAQxNZb8j/p/2DrBBLmOXHP0VemCUZ5FdifeHbg="
    ];
  };
}
```

После этого примените конфигурацию NixOS обычным rebuild. Если нужный output Lavis уже есть в кеше, Nix скачает его с `lavis.cachix.org` вместо сборки из исходников.

Импортируйте модуль и включите сервис:

```nix
{
  imports = [ inputs.lavis.nixosModules.default ];

  services.lavis = {
    enable = true;
    autoStart = false;
    credentialsEnvironmentFile = "/run/secrets/lavis.env";
  };
}
```

По умолчанию модуль создаёт отдельного системного пользователя `lavis` с home `/var/lib/lavis`. После применения конфигурации выполните авторизацию через поставляемый helper и запустите сервис:

```bash
sudo lavis-auth
sudo systemctl start lavis.service
```

После успешной авторизации можно включить `autoStart`.

Декларативные внешние модули также поддерживаются:

```nix
services.lavis.extensions = [
  {
    id = "gaf";
    package = inputs.lavis.packages.${pkgs.system}.lavis-extension-gaf;
  }
];
```

Полное описание сервиса, recovery и declarative extensions: [NixOS module](docs/nixos-module.md).

---

## Любой другой Linux

Тот же flake собирает outputs для `x86_64-linux` и `aarch64-linux`, так что Lavis
работает на любом Linux-дистрибутиве с Nix и включёнными флейками — NixOS не
обязателен. Бинарный пакет полностью замкнут в Nix store, `fastfetch` уже прописан
в `PATH` обёртки.

Добавь бинарный кеш в `~/.config/nix/nix.conf`, чтобы будущие обновления
скачивались, а не компилировались локально (aarch64-артефакты обычно ещё не в кеше):

```ini
extra-substituters = https://lavis.cachix.org
extra-trusted-public-keys = lavis.cachix.org-1:EXJoSAQxNZb8j/p/2DrBBLmOXHP0VemCUZ5FdifeHbg=
```

Ставим и один раз авторизуемся интерактивно:

```bash
nix profile install 'git+https://tangled.org/zumuvik.tngl.sh/lavis'
lavis auth
```

Запускаем как systemd user-сервис в `~/.config/systemd/user/lavis.service`:

```ini
[Unit]
Description=Lavis Telegram userbot
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=%h/.nix-profile/bin/lavis run
WorkingDirectory=%h
Environment=LAVIS_SERVICE=1
Restart=on-failure
# Код 78 означает, что нужна интерактивная повторная авторизация — перезапуск его не лечит.
RestartPreventExitStatus=78
RestartSec=5s

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now lavis.service
loginctl enable-linger "$USER"
```

Обновление: `nix profile upgrade '.*'`, затем `systemctl --user restart lavis.service`;
откат — `nix profile rollback`. Состояние живёт в XDG-каталогах ниже и переживает обновления.

---

## Локальные данные

Lavis хранит изменяемые данные вне Nix store в XDG-каталогах:

```text
$XDG_CONFIG_HOME/lavis/       # credentials, companion token, Fastfetch profile
$XDG_STATE_HOME/lavis/        # MTProto session, settings, aliases и runtime state
$XDG_DATA_HOME/lavis/         # установленные модули и staging data
```

Если XDG-переменные не заданы, используются каталоги внутри `~/.config`, `~/.local/state` и `~/.local/share`.

---

## Разработка

```bash
git clone https://tangled.org/zumuvik.tngl.sh/lavis
cd lavis
nix develop
cargo run
```

Проверка проекта:

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
nix flake check --print-build-logs
nix build --print-build-logs
```

---

## Документация

| Документ | Содержание |
| --- | --- |
| [Companion bot setup](docs/companion-bot-setup.md) | BotFather flow, workspace и восстановление |
| [External modules](docs/external-modules.md) | Жизненный цикл и runtime-модель внешних модулей |
| [`.lmod` packaging](docs/lmod-packaging.md) | Формат упаковки и правила проверки |
| [Module API v1](docs/module-api-v1.md) | Ранний API и метаданные модулей |
| [Module API v2](docs/module-api-v2.md) / [v3](docs/module-api-v3.md) | Manifest и развитие event protocol |
| [Module API v4](docs/module-api-v4.md) | Редактирование сообщений и наборы реакций |
| [Module API v5](docs/module-api-v5.md) | Gateway статуса Telegram-аккаунта |
| [Module API v6](docs/module-api-v6.md) | Capability-based Telegram RPC и raw MTProto boundary |
| [NixOS module](docs/nixos-module.md) | Декларативная настройка сервиса |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Разработка и участие в проекте |

---

## Безопасность

Lavis имеет доступ к авторизованной Telegram-сессии.

- не запускайте недоверенные сборки Lavis;
- не устанавливайте неизвестные внешние модули;
- не публикуйте API credentials и файлы сессии;
- не передавайте XDG-каталоги Lavis;
- используйте `RUST_LOG=lavis=debug` вместо глобального debug для зависимостей;
- учитывайте, что Fastfetch может раскрывать информацию о хосте.

---

## Статус проекта

Lavis активно развивается и пока имеет статус **alpha**. Module API, формат `.lmod`, схема persistence, команды и Nix-интерфейсы могут меняться несовместимо.

Lavis является неофициальным клиентом Telegram. Использование userbot может привести к ограничениям или потере аккаунта. Пользователь самостоятельно отвечает за свой Telegram-аккаунт, установленные модули, credentials и соблюдение правил Telegram и применимого законодательства.

---

## Лицензия

Copyright © 2026 zumuvik.

Lavis распространяется под лицензией [GNU GPL-3.0-only](LICENSE).
