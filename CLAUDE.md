# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

GlobalProtect VPN client built on OpenConnect (Rust workspace, edition 2024, toolchain pinned to 1.91 via `rust-toolchain.toml`). Targets Windows (x64/ARM64), macOS (Apple silicon), Linux, FreeBSD, OpenBSD. `gpclient` (CLI) does not build on Windows; on Windows the tunnel runs inside the `GPConnect` service (`gpservice`).

The official GP Connect desktop GUI (`gpgui`) and Windows `gp-posture.exe` are proprietary (private repo). This fork adds open-source GUIs: `apps/gpgui-win` (Windows, Win32 + Direct2D; branch `windows-gui`) and `apps/gpgui-gtk` (Linux, branch `gtk-gui`), both called "GlobalProtect VPN", sharing logic in `crates/gpdesktop`.

### Windows build (this machine)

Toolchain: rustup (MSVC), VS 2022 Build Tools (C++), MSYS2 UCRT64 (`gcc pkgconf gnutls libxml2 lz4` + `base-devel autoconf automake libtool gettext`), NSIS 3. In PowerShell prepend `$env:USERPROFILE\.cargo\bin` to `PATH`.

```powershell
cargo build -p gpgui-win                     # GUI only; needs no OpenConnect
cargo test -p gpdesktop                      # shared desktop core
powershell -ExecutionPolicy Bypass -File scripts\build-gpgui-win.ps1   # OpenConnect + gpservice + gpauth + gpgui-win + NSIS installer
```

`gpservice` on Windows links the MinGW-built `libopenconnect-5.dll` via `OPENCONNECT_WINDOWS_PREFIX` (set by the script; see `crates/openconnect/build/windows.rs`). It only runs under the Service Control Manager, so end-to-end tests need the installer (`packaging/windows-oss`). GUI visuals can be checked without the service: `GPGUI_WIN_SCREENSHOT=out.png GPGUI_WIN_DEMO=connected gpgui-win.exe` (see `apps/gpgui-win/README.md`).

## Setup

```bash
git submodule update --init --recursive   # required: crates/openconnect/deps/{openconnect,libxml2}
```

`crates/openconnect/build.rs` builds OpenConnect and libxml2 from those submodules with autotools, so the build host needs autoconf/automake/gettext/libtool/pkg-config plus gnutls, p11-kit, nettle, gmp, zlib, lz4, libxml2 dev packages. Set `LIBXML2_STATIC=1` to link libxml2 statically (CI does this). Tauri prerequisites are needed for `gpauth` (webview) and `gpgui-helper`.

DevContainer build:
```bash
docker build -t gpoc-devcontainer .devcontainer/
docker run --privileged --cap-add=NET_ADMIN --device=/dev/net/tun --tty -v "$(pwd)":/workspace -w /workspace gpoc-devcontainer \
  bash -c "export PATH=/usr/local/cargo/bin:\$PATH && make build"
```

## Commands

```bash
make build                         # release build of gpclient, gpservice, gpauth, gpgui-helper
make build BUILD_GUI_HELPER=0      # skip gpgui-helper (no Node/pnpm needed)
make build BUILD_WEBVIEW_AUTH=0    # gpauth without embedded webview (--no-default-features)
make build INCLUDE_GUI=1           # also download the matching published proprietary gpgui
make dev-service                   # scripts/dev-service.sh: debug gpservice in --dev-standalone mode (run as desktop user; uses sudo)

cargo test --workspace --locked                       # full test suite (what CI runs, as non-root)
cargo test -p gpapi                                   # one crate
cargo test -p gpapi <module::path::test_name> -- --exact   # one test
cargo test -p gpservice --bin gpservice device_anchor::tests  # tests inside a binary crate
```

A few tests only exercise their real paths as root; CI runs them separately with `sudo -E env PATH="$PATH" PKEXEC_UID="$(id -u)" cargo test ... -- --exact` (see `.github/workflows/build.yaml`, job `test-gp`).

Release/packaging scripts have Python unit tests: `python3 -m unittest discover -s scripts/tests -p 'test_*.py'`. Shell scripts are checked with `shellcheck` / `bash -n` in `release-checks.yaml`.

Packaging targets: `make tarball | deb | rpm | pkgbuild | apk | binary` (with `OFFLINE=1` vendoring deps). `make install [DESTDIR=...]` dispatches to `install-linux` / `install-bsd` / `install-macos`.

## Formatting

- Rust: `rustfmt.toml` — **2-space indent**, `max_width = 120`, Unix newlines. Run `cargo fmt`.
- C (`crates/openconnect/src/ffi/*.c`, tests `*.c`): `.clang-format` — Linux kernel style, tabs, 80 cols.
- `apps/gpgui-helper` frontend: React + TypeScript + Vite, pnpm, eslint/prettier.

## Architecture

### Binaries (`apps/`)

- **gpclient** — CLI (`connect`, `disconnect`, `launch-gui`, `hip`). Drives portal/gateway login via `gpapi`, then runs the tunnel through `gptunnel`/`openconnect`. Runs privileged (`sudo`); spawns `gpauth` as the desktop user for browser/webview auth. Uses PID lock `/var/run/gpclient.lock`. Also used by `gpservice` as a privileged helper (`GP_CLIENT_BINARY`).
- **gpservice** — root background service the desktop GUI talks to. WebSocket server (`ws_server.rs`, `ws_connection.rs`, `routes.rs`, `request_dispatcher.rs`) with a Noise-encrypted transport; owns the VPN task (`vpn_task/`), HIP approval/preview, session registry, and per-platform pieces (`macos_broker.rs` on macOS, `device_anchor.rs` on Linux/BSD). Debug builds add `dev_bootstrap` / `credential_lease` used by `--dev-standalone`.
- **gpauth** — Tauri app for SAML/SSO: embedded webview (`webview_auth.rs`, feature-gated) or external browser with a local callback (`callback_app.rs`). Emits the auth result back to the caller.
- **gpgui-helper** — Tauri + React app that downloads/updates/installs the proprietary `gpgui` binary (`src-tauri/src/{downloader,updater}.rs`).
- **gpgui-win** — open-source Windows GUI. Thin Win32 shell: `src/win/scene.rs` is a small custom-drawn widget set (native borderless EDITs for text input), `mod.rs` owns the message loop and an `App` in a `thread_local` `RefCell`; window-proc handlers return `Effect`s that run after the borrow is released (menus, new windows) to avoid re-entrancy. Stub `main` on non-Windows.

### GUI ↔ service protocol (used by gpgui, gpgui-gtk, gpgui-win)

Bootstrap a `SessionCredential` and keep a lease: Windows — admission pipe `\\.\pipe\GPConnect.Service`, write `GPC1`, read the frame, keep the pipe open; Linux — `pkexec gpservice --desktop-credentials`, frame on stdout, keep stdin open. Then WebSocket `ws://127.0.0.1:<port>/ws` (port from the service lock file), plaintext `ClientPrelude` (product_version must equal the service's), Noise NNpsk0 (PSK = credential secret), `Attach`, then encrypted `Request`/`Reply`/`Event`. The GUI does all portal/gateway authentication itself and sends `WsRequest::Connect(ConnectRequest)` with an authenticated `ConnectionPlan`. Implementation: `crates/gpdesktop/src/service/`.

### Shared crates (`crates/`)

- **gpapi** — core library used by all binaries: portal (`prelogin`, `config`) and gateway (`login`, session) HTTP protocol, credentials, cookie store, HIP, OS profile, process launching (`process/`: running auth/GUI as the desktop user from a root process, desktop session env, crash/rendering recovery), and the service protocol (`service/`: requests, events, VPN state, `transport/` Noise handshake + message framing shared by gpservice and GUI clients). Optional features: `clap`, `logger`.
- **openconnect** — Rust FFI over a forked libopenconnect (`globalprotect` branch submodule). C glue in `src/ffi/vpn.c`; C+Rust integration tests in `tests/`.
- **gptunnel** — tunnel configuration and monitoring on top of `openconnect`.
- **gpvpnc** — vpnc-script handling (embedded macOS script).
- **gphip** — HIP report generation (askama templates in `templates/`; macOS/unix sentinels).
- **auth** — browser / webview / callback auth flows used by `gpauth`.
- **browser-launcher** — per-platform default/named browser launching.
- **gpdesktop** — UI-agnostic desktop core shared by the open-source GUIs: `auth.rs` (portal → gateway sign-in, mirrors `gpclient/src/connect`), `service/` (client above), `controller.rs` (connection state machine: status, auto reconnect with back-off, `ReconnectRequired` re-auth, stray-process handling; front ends send `Command`s and render `UiEvent`s), `config.rs` (settings JSON), `secrets.rs` (Windows Credential Manager), `procs/`.
- **common** — constants and binary path resolution (`binary_paths.rs`): each helper binary (`gpauth`, `gpclient`, `gpservice`, `gpgui`, `gpgui-helper`) resolves as: env var override (`GP_AUTH_BINARY`, `GP_CLIENT_BINARY`, ...) → sibling of the current executable → compiled-in default install path.

Privilege model to keep in mind: tunnel setup is root (`gpclient` under sudo / `gpservice`), while authentication UI and browsers must run as the desktop user — much of `gpapi::process` exists to cross that boundary safely. Changes there are security-sensitive and have root-only tests.

## Other references

- `docs/bsd-source-build.md` — FreeBSD/OpenBSD builds
- `docs/diagnostics*.md`, `docs/design/webkit-crash-recovery.md`, `docs/webkit-rendering.md`
- `flake.nix` / `nix/` — Nix packaging; `packaging/` — distro packaging files
- `changelog.md`
