<div align="center">

<img src="assets/logo.png" alt="Lavis logo" width="240">

# Lavis

**Быстрый и расширяемый Telegram userbot на Rust**

Работает напрямую через MTProto, ориентирован на Linux и предоставляет полноценную декларативную интеграцию с NixOS.

<p>
  <a href="https://github.com/zumuvik/lavis/actions/workflows/ci.yml">
    <img src="https://github.com/zumuvik/lavis/actions/workflows/ci.yml/badge.svg" alt="CI">
  </a>
  <a href="LICENSE">
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

Он обрабатывает команды непосредственно через ваш Telegram-аккаунт, редактирует исходные сообщения ответами и поддерживает как встроенные команды, так и внешние модули на любом языке.

```text
,ping
,stats
,fastfetch
,help
```

Префикс по умолчанию — запятая `,`, но его можно изменить.

---

## Возможности

| Возможность                     | Описание                                                          |
| ------------------------------- | ----------------------------------------------------------------- |
| ⚡ **Нативное ядро на Rust**     | Асинхронная работа с Telegram через MTProto без Bot API           |
| 🔐 **Локальная авторизация**    | Сеанс и API credentials хранятся локально с ограниченными правами |
| ✏️ **Редактирование сообщений** | Результат команды заменяет исходное сообщение                     |
| 🧩 **Внешние модули**           | Модули на любом языке через JSON Lines protocol                   |
| 📦 **Формат `.lmod`**           | Проверяемая установка модулей через «Сохранённые сообщения»       |
| 🐧 **NixOS integration**        | Flake, пакет, dev shell и готовый NixOS-модуль                    |
| 🔧 **Префиксы и алиасы**        | Настраиваемые команды и сохраняемые псевдонимы                    |
| 🤖 **Companion bot**            | Создание и восстановление вспомогательного Telegram-бота          |
| 🖥️ **Fastfetch**               | Ограниченный и проверяемый вывод информации о системе             |
| 📚 **Контекстная справка**      | Справка по командам и модулям прямо в Telegram                    |

---

## Быстрый запуск

### 1. Получите Telegram API credentials

Создайте приложение на [my.telegram.org](https://my.telegram.org/apps) и получите:

* `API ID`;
* `API hash`.

### 2. Запустите Lavis

```bash
nix run github:zumuvik/lavis
```

При первом запуске Lavis запросит:

1. API ID и API hash;
2. номер телефона;
3. код подтверждения Telegram;
4. пароль двухфакторной аутентификации, если он включён.

После успешного входа локальный MTProto-сеанс будет использоваться автоматически.

> [!IMPORTANT]
> Не публикуйте API hash, файл `credentials.json`, базу данных сеанса или логи терминала, содержащие данные авторизации.

---

## Основные команды

| Команда     | Назначение                           |
| ----------- | ------------------------------------ |
| `help`      | Справка по командам и модулям        |
| `modules`   | Список встроенных и внешних модулей  |
| `ping`      | Проверка задержки MTProto            |
| `stats`     | Статистика и время работы            |
| `prefix`    | Просмотр и изменение префикса        |
| `alias`     | Управление псевдонимами              |
| `fastfetch` | Информация о системе                 |
| `setup`     | Настройка companion-бота             |
| `lm`        | Управление внешними `.lmod`-модулями |
| `reboot`    | Перезапуск приложения Lavis          |

<details>
<summary><b>Примеры команд</b></summary>

```text
,help
,help ping
,help lm
,modules
,ping
,stats
,prefix
,prefix .
.prefix reset
,fastfetch
,alias add sys fastfetch
,sys
```

</details>

---

## Внешние модули

Внешний модуль Lavis — это отдельная исполняемая программа, обменивающаяся с ядром JSON-строками через `stdin` и `stdout`.

Модуль можно написать на Rust, Go, Python или любом другом языке.

```bash
lavis modules validate ./my-module/module.json
lavis modules enable my-module
lavis modules disable my-module
lavis modules status
```

После включения команды модуля становятся доступны в Telegram:

```text
,my-module.command аргументы
```

### Установка `.lmod` через Telegram

1. Прикрепите `.lmod` к новому сообщению в «Сохранённых сообщениях».

2. Добавьте текст:

   ```text
   ,lm install
   ```

3. Lavis скачает архив и покажет план установки.

4. Подтвердите установку одноразовым Approval ID:

   ```text
   ,lm confirm XXXX-XXXX-XXXX-XXXX
   ```

5. Установленный модуль остаётся выключенным. Включите его и перезапустите Lavis:

   ```text
   ,lm enable <module-id>
   ,reboot
   ```

   `,lm list` показывает установленные модули, а `,lm info <module-id>` — сведения о модуле.
   `,lm disable <module-id>` отключает модуль для следующего запуска. Состояние включения
   сохраняется; горячая загрузка и живое включение/отключение не поддерживаются.

   `,reboot` перезапускает только процесс Lavis, а не систему. Команда принимается из нового
   собственного сообщения в любом чате; отредактированные сообщения не подходят.

   `,reboot` редактирует то же сообщение с командой сначала в «♻️ Lavis перезапускается…»,
   а после успешного запуска — в «✅ Lavis перезагрузился» с целым временем перезапуска в
   секундах с усечением дробной части; отдельное сообщение не создаётся.

> [!WARNING]
> Внешние модули не изолируются системной песочницей и работают с правами пользователя Lavis.
>
> Для v5 capability `telegram.account.status` принудительно проверяется ядром
> на границе gateway, но не ограничивает прямой доступ модуля к ОС.
> Устанавливайте только доверенный код.

Подробнее: [External modules](docs/external-modules.md), включая
[Module API v5](docs/module-api-v5.md) для gateway статуса аккаунта.

---

## NixOS

Добавьте Lavis в inputs вашего flake:

```nix
{
  inputs.lavis.url = "github:zumuvik/lavis";
}
```

Импортируйте модуль и включите сервис:

```nix
{
  imports = [ inputs.lavis.nixosModules.default ];

  services.lavis = {
    enable = true;
    # Keep stopped until the first interactive auth succeeds.
    autoStart = false;
    credentialsEnvironmentFile = "/run/secrets/lavis.env";
  };
}
```

По умолчанию модуль создаёт системного пользователя `lavis` и хранит данные в
`/var/lib/lavis`. После применения конфигурации выполните интерактивную
авторизацию тем же окружением, которое использует systemd-сервис:

```bash
sudo lavis-auth
sudo systemctl start lavis.service
```

После успешной авторизации можно убрать `autoStart = false` или заменить на
`autoStart = true`.

Full service, credentials and declarative extension setup:
[NixOS module](docs/nixos-module.md).

Декларативно также можно подключать внешние модули:

```nix
services.lavis.extensions = [
  {
    id = "gaf";
    package = inputs.lavis.packages.x86_64-linux.lavis-extension-gaf;
  }
];
```

Такие модули управляются декларативно: изменяйте `services.lavis.extensions` в конфигурации
и применяйте NixOS rebuild, например `nh os switch`. Telegram-команды `,lm enable` и
`,lm disable` для декларативного модуля отклоняются.

Полная документация: [NixOS module](docs/nixos-module.md).

---

## Учётные данные и локальные файлы

API credentials сохраняются в:

```text
$XDG_CONFIG_HOME/lavis/credentials.json
```

При отсутствии `XDG_CONFIG_HOME` используется:

```text
$HOME/.config/lavis/credentials.json
```

Основное состояние хранится в XDG-каталогах:

```text
$XDG_STATE_HOME/lavis/
$XDG_DATA_HOME/lavis/
```

Среди локальных данных:

* MTProto-сеанс;
* настройки префикса;
* псевдонимы;
* список включённых модулей;
* установленные внешние модули.

Проверить источник credentials:

```bash
lavis credentials
```

Удалить только сохранённые API credentials:

```bash
lavis credentials reset
```

Удалить локальный Telegram-сеанс:

```bash
lavis logout
```

`logout` не отзывает сеанс на стороне Telegram. При необходимости завершите его отдельно в настройках активных сеансов Telegram.

---

## Разработка

```bash
git clone https://github.com/zumuvik/lavis
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

| Документ                                           | Содержание                               |
| -------------------------------------------------- | ---------------------------------------- |
| [Companion bot setup](docs/companion-bot-setup.md) | Создание и восстановление companion-бота |
| [Module API v1](docs/module-api-v1.md)             | Встроенные модули и метаданные команд    |
| [Module API v2/v3](docs/module-api-v2.md)          | Manifest и JSON Lines protocol           |
| [Module API v4](docs/module-api-v4.md)             | Редактирование сообщений и наборы реакций |
| [Module API v5](docs/module-api-v5.md)             | Gateway статуса аккаунта                  |
| [External modules](docs/external-modules.md)       | Разработка и запуск внешних модулей      |
| [Packaging `.lmod`](docs/lmod-packaging.md)        | Формат и безопасная упаковка `.lmod`     |
| [NixOS module](docs/nixos-module.md)               | Декларативная настройка сервиса          |
| [CONTRIBUTING.md](CONTRIBUTING.md)                 | Разработка и участие в проекте           |

Минимальный пример внешнего модуля находится в
[`examples/external-module-echo`](examples/external-module-echo).

---

## Безопасность

Lavis имеет полный доступ к Telegram-сеансу пользователя.

Основные рекомендации:

* не запускайте недоверенные сборки Lavis;
* не устанавливайте неизвестные внешние модули;
* не публикуйте API hash и файл сеанса;
* не передавайте содержимое XDG-каталогов Lavis;
* используйте `RUST_LOG=lavis=debug` вместо глобального debug-логирования зависимостей;
* учитывайте, что `fastfetch` может раскрывать сведения о системе.

Обнаруженные уязвимости не следует публиковать вместе с действующими credentials или файлами сеансов.

---

## Статус проекта

Lavis находится в активной разработке и пока не заявляется как готовый к production-использованию.

Возможны несовместимые изменения Module API, формата `.lmod`, структуры конфигурации и CLI.

---

## Отказ от ответственности

Lavis является неофициальным клиентом Telegram.

Использование userbot может привести к ограничениям или блокировке аккаунта. Пользователь самостоятельно отвечает за сохранность учётных данных, установленные модули, соблюдение правил Telegram и применимого законодательства.

Программа предоставляется «как есть», без каких-либо гарантий.

---

## Лицензия

Copyright © 2026 zumuvik

Lavis распространяется по лицензии
[GNU General Public License v3.0 only](LICENSE).

`SPDX-License-Identifier: GPL-3.0-only`