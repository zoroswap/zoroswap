# ZoroSwap

[![built for Miden][ico-miden]](https://miden.xyz/) [![Chat on Telegram][ico-telegram]][link-telegram] [![Chat on Twitter][ico-twitter]][link-twitter]

[ico-miden]: https://img.shields.io/badge/built%20for-Miden-orange.svg?logo=data:image/svg%2bxml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9Im5vIj8+CjxzdmcKICAgZmlsbD0ibm9uZSIKICAgY2xhc3M9ImgtOSBzaHJpbmstMCIKICAgdmlld0JveD0iMCAwIDI2LjAwMDk5OSAzNiIKICAgdmVyc2lvbj0iMS4xIgogICBpZD0ic3ZnMiIKICAgc29kaXBvZGk6ZG9jbmFtZT0ibWlkZW4tbG9nby5zdmciCiAgIHdpZHRoPSIyNi4wMDA5OTkiCiAgIGhlaWdodD0iMzYiCiAgIGlua3NjYXBlOnZlcnNpb249IjEuMy4yICgwOTFlMjBlLCAyMDIzLTExLTI1KSIKICAgeG1sbnM6aW5rc2NhcGU9Imh0dHA6Ly93d3cuaW5rc2NhcGUub3JnL25hbWVzcGFjZXMvaW5rc2NhcGUiCiAgIHhtbG5zOnNvZGlwb2RpPSJodHRwOi8vc29kaXBvZGkuc291cmNlZm9yZ2UubmV0L0RURC9zb2RpcG9kaS0wLmR0ZCIKICAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogICB4bWxuczpzdmc9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogICB4bWxuczpyZGY9Imh0dHA6Ly93d3cudzMub3JnLzE5OTkvMDIvMjItcmRmLXN5bnRheC1ucyMiCiAgIHhtbG5zOmNjPSJodHRwOi8vY3JlYXRpdmVjb21tb25zLm9yZy9ucyMiCiAgIHhtbG5zOmRjPSJodHRwOi8vcHVybC5vcmcvZGMvZWxlbWVudHMvMS4xLyI+CiAgPGRlZnMKICAgICBpZD0iZGVmczIiIC8+CiAgPHNvZGlwb2RpOm5hbWVkdmlldwogICAgIGlkPSJuYW1lZHZpZXcyIgogICAgIHBhZ2Vjb2xvcj0iI2ZmZmZmZiIKICAgICBib3JkZXJjb2xvcj0iIzAwMDAwMCIKICAgICBib3JkZXJvcGFjaXR5PSIwLjI1IgogICAgIGlua3NjYXBlOnNob3dwYWdlc2hhZG93PSIyIgogICAgIGlua3NjYXBlOnBhZ2VvcGFjaXR5PSIwLjAiCiAgICAgaW5rc2NhcGU6cGFnZWNoZWNrZXJib2FyZD0iMCIKICAgICBpbmtzY2FwZTpkZXNrY29sb3I9IiNkMWQxZDEiCiAgICAgaW5rc2NhcGU6em9vbT0iNS45MDQzNDc4IgogICAgIGlua3NjYXBlOmN4PSIyMi4yNzE3MjMiCiAgICAgaW5rc2NhcGU6Y3k9IjIwLjc0NzQyMyIKICAgICBpbmtzY2FwZTp3aW5kb3ctd2lkdGg9IjEzOTIiCiAgICAgaW5rc2NhcGU6d2luZG93LWhlaWdodD0iMTIxMiIKICAgICBpbmtzY2FwZTp3aW5kb3cteD0iMCIKICAgICBpbmtzY2FwZTp3aW5kb3cteT0iMzEiCiAgICAgaW5rc2NhcGU6d2luZG93LW1heGltaXplZD0iMCIKICAgICBpbmtzY2FwZTpjdXJyZW50LWxheWVyPSJzdmcyIiAvPgogIDx0aXRsZQogICAgIGlkPSJ0aXRsZTEiPk1pZGVuPC90aXRsZT4KICA8cGF0aAogICAgIGZpbGw9IiNmZjU1MDAiCiAgICAgZD0ibSAxMi45MzIsMTAuNjU2IDIuMTU1LDIuMzcyIGMgMC4xODQsMC4yMDIgMC41MiwwLjA3MiAwLjUyLC0wLjIgdiAtNS42OCBhIDAuMywwLjMgMCAwIDEgMC4yOTgsLTAuMyBoIDMuOTYgYyAxLjczOCwwIDMuMzQ4LDAuOTA4IDQuMjM5LDIuMzkgbCAwLjc4MiwxLjMgYSAwLjU4NSwwLjU4NSAwIDAgMCAwLjUsMC4yODMgaCAwLjAzIEEgMC41ODUsMC41ODUgMCAwIDAgMjYsMTAuMjM2IFYgNC4zMjggQSAwLjU4NSwwLjU4NSAwIDAgMCAyNS40MTYsMy43NDMgSCAxNi4yMDQgQSAwLjU5NywwLjU5NyAwIDAgMSAxNS42MDcsMy4xNDYgViAwLjQ0OCBDIDE1LjYwNywwLjIwMSAxNS40MDcsMCAxNS4xNTksMCBIIDEzLjA3IGMgLTAuMjQ3LDAgLTAuNDQ4LDAuMiAtMC40NDgsMC40NDggdiAyLjY5OCBjIDAsMC4zMyAtMC4yNjcsMC41OTggLTAuNTk3LDAuNTk4IEggMC41ODUgQSAwLjU4NSwwLjU4NSAwIDAgMCAwLDQuMzI3IHYgMS4zOTQgYyAwLDAuMTUgMC4wNTgsMC4yOTQgMC4xNjEsMC40MDMgbCAxMi44MzcsMTEuNDc0IGEgMC41ODMsMC41ODMgMCAwIDEgMCwwLjgwMiBMIDAuMTYsMjkuODc1IGEgMC41ODYsMC41ODYgMCAwIDAgLTAuMTYxLDAuNDAzIHYgMS4zOTQgYyAwLDAuMzIzIDAuMjYyLDAuNTg0IDAuNTg1LDAuNTg0IGggMTEuNDQgYyAwLjMzLDAgMC41OTcsMC4yNjggMC41OTcsMC41OTggdiAyLjY5OCBjIDAsMC4yNDcgMC4yLDAuNDQ4IDAuNDQ4LDAuNDQ4IGggMi4wODkgYyAwLjI0NywwIDAuNDQ4LC0wLjIgMC40NDgsLTAuNDQ4IHYgLTIuNjk4IGMgMCwtMC4zMyAwLjI2NywtMC41OTcgMC41OTYsLTAuNTk3IGggOS4yMTIgQSAwLjU4NSwwLjU4NSAwIDAgMCAyNiwzMS42NyB2IC01LjkwNyBhIDAuNTg1LDAuNTg1IDAgMCAwIC0wLjU4NSwtMC41ODUgaCAtMC4wMjggYSAwLjU4NSwwLjU4NSAwIDAgMCAtMC41MDIsMC4yODMgbCAtMC43ODEsMS4zIGEgNC45NDQsNC45NDQgMCAwIDEgLTQuMjM4LDIuMzkgaCAtMy45NjEgYSAwLjI5OSwwLjI5OSAwIDAgMSAtMC4yOTgsLTAuMyB2IC01LjY4IGEgMC4yOTgsMC4yOTggMCAwIDAgLTAuNTIsLTAuMiBsIC0yLjE1NSwyLjM3MiBjIC0wLjIsMC4yMiAtMC4zMSwwLjUwNiAtMC4zMSwwLjgwNCB2IDIuNzA1IEEgMC4yOTgsMC4yOTggMCAwIDEgMTIuMzI0LDI5LjE1IEggNC45OCBBIDAuMDk2LDAuMDk2IDAgMCAxIDQuODk2LDI5LjEwNSAwLjA5NCwwLjA5NCAwIDAgMSA0LjkwNSwyOC45OTcgTCAxNi4zNDgsMTguMjQ0IDE2LjM1LDE4LjI0MSBhIDAuMzg1LDAuMzg1IDAgMCAwIDAsLTAuNDg1IEwgMTYuMzQ4LDE3Ljc1NCA0LjkwNSw3LjAwMiBBIDAuMDk0LDAuMDk0IDAgMCAxIDQuODk2LDYuODk0IDAuMDk2LDAuMDk2IDAgMCAxIDQuOTgsNi44NDkgaCA3LjM0NCBjIDAuMTY0LDAgMC4yOTgsMC4xMzMgMC4yOTgsMC4yOTggdiAyLjcwNSBjIDAsMC4yOTcgMC4xMSwwLjU4NCAwLjMxLDAuODA0IHoiCiAgICAgc3R5bGU9ImZpbGwtb3BhY2l0eToxIgogICAgIGlkPSJwYXRoMSIgLz4KICA8bWV0YWRhdGEKICAgICBpZD0ibWV0YWRhdGEyIj4KICAgIDxyZGY6UkRGPgogICAgICA8Y2M6V29yawogICAgICAgICByZGY6YWJvdXQ9IiI+CiAgICAgICAgPGRjOnRpdGxlPk1pZGVuPC9kYzp0aXRsZT4KICAgICAgPC9jYzpXb3JrPgogICAgPC9yZGY6UkRGPgogIDwvbWV0YWRhdGE+Cjwvc3ZnPgo=
[ico-telegram]: https://img.shields.io/badge/TG-ZoroSwap-blue.svg?logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI+PHBhdGggZD0iTTEyIDI0YzYuNjI3IDAgMTItNS4zNzMgMTItMTJTMTguNjI3IDAgMTIgMCAwIDUuMzczIDAgMTJzNS4zNzMgMTIgMTIgMTJaIiBmaWxsPSJ1cmwoI2EpIi8+PHBhdGggZmlsbC1ydWxlPSJldmVub2RkIiBjbGlwLXJ1bGU9ImV2ZW5vZGQiIGQ9Ik01LjQyNSAxMS44NzFhNzk2LjQxNCA3OTYuNDE0IDAgMCAxIDYuOTk0LTMuMDE4YzMuMzI4LTEuMzg4IDQuMDI3LTEuNjI4IDQuNDc3LTEuNjM4LjEgMCAuMzIuMDIuNDcuMTQuMTIuMS4xNS4yMy4xNy4zMy4wMi4xLjA0LjMxLjAyLjQ3LS4xOCAxLjg5OC0uOTYgNi41MDQtMS4zNiA4LjYyMi0uMTcuOS0uNSAxLjE5OS0uODE5IDEuMjI5LS43LjA2LTEuMjI5LS40Ni0xLjg5OC0uOS0xLjA2LS42ODktMS42NDktMS4xMTktMi42NzgtMS43OTgtMS4xOS0uNzgtLjQyLTEuMjA5LjI2LTEuOTA4LjE4LS4xOCAzLjI0Ny0yLjk3OCAzLjMwNy0zLjIyOC4wMS0uMDMuMDEtLjE1LS4wNi0uMjEtLjA3LS4wNi0uMTctLjA0LS4yNS0uMDItLjExLjAyLTEuNzg4IDEuMTQtNS4wNTYgMy4zNDgtLjQ4LjMzLS45MDkuNDktMS4yOTkuNDgtLjQzLS4wMS0xLjI0OC0uMjQtMS44NjgtLjQ0LS43NS0uMjQtMS4zNDktLjM3LTEuMjk5LS43OS4wMy0uMjIuMzMtLjQ0Ljg5LS42NjlaIiBmaWxsPSIjZmZmIi8+PGRlZnM+PGxpbmVhckdyYWRpZW50IGlkPSJhIiB4MT0iMTEuOTkiIHkxPSIwIiB4Mj0iMTEuOTkiIHkyPSIyMy44MSIgZ3JhZGllbnRVbml0cz0idXNlclNwYWNlT25Vc2UiPjxzdG9wIHN0b3AtY29sb3I9IiMyQUFCRUUiLz48c3RvcCBvZmZzZXQ9IjEiIHN0b3AtY29sb3I9IiMyMjlFRDkiLz48L2xpbmVhckdyYWRpZW50PjwvZGVmcz48L3N2Zz4K&url=https%3A%2F%2Ft.me%2FZoroSwap%2F1
[ico-twitter]: https://img.shields.io/twitter/url?label=ZoroSwap&style=social&url=https%3A%2F%2Ftwitter.com%2FZoroSwap

[link-telegram]: https://t.me/+KyKHHuIxxPdmOTky
[link-twitter]: https://twitter.com/zoroswap

This repository contains an Oracle-informed AMM for the [Miden blockchain](https://miden.xyz/).

You can find our testnet deployment here: [https://zoroswap.com](https://zoroswap.com).

## Setup

### 1. Copy environment and app configs

```sh
cp .env-example .env
cp config-example.toml config.toml
cp faucets-example.toml faucets.toml
```

### 2. Spawn faucets

```sh
cargo run --release --bin spawn_faucets
```

It will print out 2 faucet ids. Put them in corresponding entries `faucet_id` under `[[liquidity_pools]]` section in `config.toml` file.

### 3. Spawn pool

```sh
cargo run --release --bin spawn_pools
```

It will print out pool id (at the start of program output). Put it into corresponding entry `pool_account_id` section in `config.toml` file.

### 4. Run the server

```sh
cargo run --release --bin server
```

## End-to-End Tests

### 1. Copy store to testing store

```sh
cp store.sqlite3 testing_store.sqlite3
```

This is required so client on server and client on test do not share the same store as then notes commited in test would go into `output_notes` but the server expects notes in `input_notes`.

### 2. Run the e2e test

Via public note:
```sh
cargo test --release e2e_public_note -- --exact --nocapture
```

Private note via endpoint:
```sh
cargo test --release e2e_private_note -- --exact --nocapture
```

1. Create an account with a basic wallet component (user).
2. Mint some token from faucets we created at setup.
3. Create a `ZOROSWAP` note requesting a swap of minted assets in line with current oracle prices and send it as a public note to testnet or via endpoint.
4. Server should pick up the emitted note, consume it and execute against the pool we created earlier and emit new `P2ID` targeted at the user with swapped assets.
5. Consume the `P2ID` emitted by the server concluding the test.

## Curve Setup

Zoro uses a pluggable curve implementation for AMM calculations. By default, it uses a simple
linear curve ([`DummyCurve`](./crates/zoro_primitives/src/dummy_curve.rs)) to demonstrate 
its functionality.

| Curve | Feature Flag                                                         | Repository                                                                 | Use Case |
|-------|----------------------------------------------------------------------|----------------------------------------------------------------------------|----------|
| **`DummyCurve`** | `default` (always available)                                         | [Public in `zoro_primitives`](./crates/zoro_primitives/src/dummy_curve.rs) | Development, testing, reference |
| **`ZoroCurve`** | `--features zoro-curve-local` or `--features zoro-curve-private-repo` | Private                          | Production, proprietary algorithm |

### Using the Default Curve

The default build uses the `DummyCurve` implementation:

```sh
cargo build
```

**Note:** The (linear) dummy curve is just here to demonstrate the functionality!

### Using the Proprietary Curve

#### From the private `zoro-curve` repository (installation on a server)

```sh
cargo run --bin server --features zoro-curve-private-repo
cargo test --features zoro-curve-private-repo
cargo install --features zoro-curve-private-repo
```

#### From a local folder (local development)

Requires a folder structure where both repositories are on the same level:

```
zoroswap/
└── ...
zoro-curve/
└── ...
```

```sh
cargo run --bin server --features zoro-curve-local
cargo test --features zoro-curve-local
cargo install --features zoro-curve-local
```