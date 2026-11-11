# CSQTT: нативный клиент для macOS (CLI + utun)

`rust-client` умеет работать на macOS в нативном режиме: вместо Android-режима
(`--tun-uds`, где TUN-дескриптор передаёт Kotlin-приложение через abstract UDS)
или UDP-режима (`--listen`) клиент сам поднимает системный интерфейс **utun**
и гоняет через него сырые IP-пакеты.

## Требования

- macOS 12+ (x86_64 и Apple Silicon), запуск — **только под root**: создание
  utun-интерфейса и настройка маршрутов требуют привилегий:

  ```bash
  sudo ./client ... --tun-utun
  ```

- Остальные аргументы те же, что и в Android-режиме: `--peer`, `--vk`/
  `--vk-hash-mode`, `CSQTT_PASSWORD` и т.д.

## Сборка

```bash
cd rust-client
cargo build --release --target aarch64-apple-darwin   # Apple Silicon
cargo build --release --target x86_64-apple-darwin    # Intel
# universal2:
lipo -create -output csqtt-client-macos-universal2 \
  target/aarch64-apple-darwin/release/client \
  target/x86_64-apple-darwin/release/client
```

CI собирает universal2-бинарь и прикрепляет его к прогону workflow
`.github/workflows/macos.yml` (артефакт `csqtt-client-macos-universal2`).

## Режимы маршрутизации

### Split-tunnel (по умолчанию, рекомендуется)

```bash
sudo CSQTT_PASSWORD=... ./client --peer ... --vk ... --tun-utun
```

После того как сервер пришлёт TUNCONF (клиентский IP и DNS), клиент:

1. Настраивает адрес: `ifconfig utunN inet <ip> <utun-peer> up`
   (`--utun-peer`, по умолчанию `10.66.67.1` — адрес серверной стороны).
2. Добавляет маршрут до подсети туннеля `<a.b.c>.0/24` через `-interface utunN`.
3. Системный DNS **не трогает** — DNS-сервер из TUNCONF только логируется.

В систему добавляется один маршрут до подсети туннеля; весь остальной трафик
идёт напрямую. TURN-endpoint'ы физически не могут попасть в туннель
(их подсеть никогда не маршрутизируется в utun), поэтому routing loop
невозможен.

### Full-tunnel (`--utun-full-tunnel`, экспериментально)

```bash
sudo CSQTT_PASSWORD=... ./client ... --tun-utun --utun-full-tunnel
```

Дополнительно добавляются маршруты `0.0.0.0/1` и `128.0.0.0/1` через utun.
Чтобы собственные транспортные соединения не попали в туннель, перед этим
через шлюз по умолчанию (`route -n get default`) добавляются более
специфичные маршруты:

- `/32` до IP пира (`--peer`) и до всех TURN-endpoint'ов, заданных
  IP-литералами (`--turn` / `--custom-turn-ips`);
- `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` —
  чтобы локальная сеть и LAN-DNS остались доступны.

Ограничения full-tunnel:

- Защита покрывает только endpoint'ы, заданные **IP-литералами**. Если TURN
  задан доменом, его IP не исключается (в лог пишется предупреждение) —
  возможна петля маршрутизации. Используйте split-режим.
- Шлюз по умолчанию определяется один раз — при применении TUNCONF. Если во
  время работы сменилась сеть (переподключение Wi-Fi, другой шлюз), защитные
  маршруты endpoint'ов остаются указывать на старый шлюз, и транспорт клиента
  может остановиться. Перезапустите клиент после смены сети.
- IPv6-трафик туннелем не захватывается (только IPv4).
- DNS-сервер из TUNCONF системно не применяется.

## Завершение и cleanup

Штатное завершение — Ctrl-C (SIGINT), **SIGTERM** (`kill <pid>`) и **SIGHUP**:
все три сигнала инициируют graceful shutdown, и клиент удаляет все добавленные
маршруты. Интерфейс utun исчезает сам при закрытии дескриптора — вместе с
interface-маршрутами.

При `kill -9` (SIGKILL) cleanup не выполняется: utun-интерфейс всё равно
будет уничтожен ядром, но gateway-маршруты `/32` до endpoint'ов (и
исключения LAN в full-tunnel) могут остаться. Они не ломают сеть, но их
можно удалить вручную:

```bash
sudo route -n delete -host <ip> <шлюз>
```

## Прочие ограничения

- `--tun-utun` несовместим с `--tun-uds`.
- MTU интерфейса задаётся `--utun-mtu` (по умолчанию 1500).
- 4-байтовый заголовок utun (address family) снимается/добавляется crate'ом
  `tun` — dispatcher получает чистые IP-пакеты, как и в Android-режиме.
