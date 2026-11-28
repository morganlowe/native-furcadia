<div align="center">
  <img src="native-icon.png" alt="Furcadia Native Client icon" width="96" />
  <h1>Furcadia Native Client</h1>
  <p><strong>A fast, standalone native desktop client for exploring Furcadia dreams, trading, roleplaying, and keeping up with the people you know — on Linux, Windows, and macOS, with no Wine required.</strong></p>
  <p>
    <img src="https://img.shields.io/github/v/release/morganlowe/native-furcadia?label=Version&style=flat-square" alt="Latest release" />
    <img src="https://img.shields.io/badge/License-CC%20BY--NC--SA%204.0-blue?style=flat-square" alt="CC BY-NC-SA 4.0 license" />
    <img src="https://img.shields.io/badge/Platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey?style=flat-square" alt="Linux, Windows, and macOS" />
    <img src="https://img.shields.io/badge/Stack-Rust%20%7C%20egui-orange?style=flat-square&logo=rust&logoColor=white" alt="Built with Rust and egui" />
  </p>
  <p>
    <a href="https://github.com/morganlowe/native-furcadia/releases">Releases</a>
    ·
    <a href="https://github.com/morganlowe/native-furcadia/issues">Report a Bug</a>
    ·
    <a href="https://github.com/morganlowe/native-furcadia/issues">Request a Feature</a>
    ·
    <a href="https://github.com/morganlowe/native-furcadia/security">Security</a>
  </p>
</div>

---

![Furcadia Native Client with fictional demo characters, Furcadia-formatted emotes and chat, whisper tabs, Pounce, and Saved Dreams](mockups/demo-chat-screenshot-reference-v3.png)

<details>
<summary><strong>Screenshots: Settings &amp; Customization (Skins, Fonts, Colors, Alerts, Cloud Sync &amp; Cache)</strong></summary>

<br />

#### Dream Window Skins, Fonts, Portrait Sizing & Full Color Customization
![Settings — Skins, fonts, portrait sizing, and live-preview chat/tab/window color pickers](mockups/settings-appearance-colors.png)

#### Animations, Whispers, Dice, Audio, Mentions & Window/Tray Behavior
![Settings — Animations, whisper and dice behavior, audio, mention alerts, taskbar flash, and system tray options](mockups/settings-behavior-alerts.png)

#### Chat & Whisper Logs, Cloud Folder Sync, Cached Downloads & Windows Importer
![Settings — Shared/per-character log retention, Nextcloud/Drive/Dropbox folder sync, avatar/dream cache manager, and Windows Furcadia importer](mockups/settings-logs-sync-cache.png)

</details>

Furcadia Native Client is an independent, natively compiled Rust client for Furcadia built with `egui`. It runs **standalone out of the box** on clean Linux, Windows, and macOS installs — automatically fetching stock modern and legacy game patches, species, dynamic Digo avatars, portraits, and dreams without requiring Wine or a pre-existing installation — while still letting you import your existing Windows or Wine Furcadia files whenever you want.

Alongside the graphical desktop client (`furc-gui`), the repository provides a script-friendly command-line client (`furc-cli`) and a reusable Rust library (`furcadia_client`) for Furcadia networking, protocol handling, and archive decoding.

## Why Furcadia Native Client?

- **True native performance & long-session stability**: compiled to a small native Rust executable for Linux, Windows, and macOS with zero Wine, compatibility layers, or browser bloat — with bounded GPU/sprite caches, capped in-memory chat history, clean tab teardown, and hardened archive/decompression limits for overnight sessions.
- **Ready on a fresh install**: connects cleanly on first launch and downloads public modern/legacy stock patches, species, dynamic avatars, portraits, and dream assets automatically.
- **Multi-alt power in one window**: keep multiple logged-in characters open simultaneously as reorderable character tabs, complete with per-character cookie counts preserved across tab switches and an accidental-close confirmation prompt when closing with multiple characters open.
- **Built for daily RP and trading**: includes dedicated **Cookie**, **Gift**, and **Trade** menus right on the player card, inline Furcadia smiley sprites, `[bracketed text]` highlighting, rich text buttons, and a configurable dice roller.
- **Clean, distraction-free UI**: crisp plain-text and ASCII interface free of emoji clutter, with customizable themes, fonts, sizes, and colors.
- **Keep everything yours**: optionally import existing maps, portraits, settings, Pounce watchlists, and logs, or sync preferences and logs across computers through your own cloud-synced folder.

## Features

<details>
<summary><strong>Playing, Dreams, and Avatars</strong></summary>

- Sign in with an account email or individual character credentials, with multi-character support in a single window.
- Render Furcadia dreams natively (`Fox5`, `FSHX`, `FSH`, and `FR01` archives; map formats `V01.40` and `V01.50`) with Furcadia palette/color remapping, animated avatars, dynamic Digo avatars, remappable/custom portraits, and player badges.
- Switch between 1x default Furcadia view range or zoomed exploration, and choose from Dream Window themes (`ClassicPurple`, `DragonHoard`, `KitterMagic`, `FerianWylde`).
- Navigate via main map shortcuts, `furc://` Dream URLs, **Saved Dreams** bookmarks, and sidebar Digo controls for wings, toggles, costumes, and Digo abilities.
- Built-in sound effects and dream music toggles.

</details>

<details>
<summary><strong>Player Card, Secure Trading, Cookies, and Gifts</strong></summary>

- Look at any player to open their portrait card with one-click **Whisper**, **Join**, **Summon**, **Follow**, and **+Pounce / -Pounce** controls.
- **Cookie menu**: run `Make Cookie`, `Give Cookie`, `Make Secret Cookie`, and `Give Secret Cookie` straight from a player's card, with per-character active and bank cookie counts preserved across alt tabs (no more stray `info` output on tab switches).
- **Gift menu**: quickly send `Give Item`, roses, bouquets, bracelets, snugs, and other items directly to the looked-at player.
- **Trade menu**: initiate and complete secure trades cleanly from the other-character card (`Start Trade`, `Trade Status`, `Add Common Item` dropdown for portraits/wings/gifts/DragonScales, inline `Item` `+Add` / `-Rem` for any custom item or currency like `GD 50`, inline `Reason` `Set`, `Accept Trade`, `Reject Trade`, and `End Trade`).
- Proper Furcadia speech-prefix (`"`) wire routing for `trade-*`, `gift-*`, `give-*`, `discard-*`, and cookie commands whether entered in Main Chat, Whisper tabs, detached whisper windows, `/cmd`, `` `cmd ``, or player menus — without breaking ordinary whisper messages like `"give me a minute"`.

</details>

<details>
<summary><strong>Chat, Whispers, and Roleplay Tools</strong></summary>

- Character-colored names and clean channel formatting across Main Chat and whispers.
- `[bracketed text]` automatically highlighted in blue inside chat and whisper messages for easy OOC / RP tag scanning.
- Inline Furcadia smiley sprites rendered in chat, player lists, Pounce rows, and the smiley picker.
- Unread **yellow** and active **light-blue** tab highlights across both character tabs and whisper tabs so missed messages stand out immediately.
- Drag-to-reorder character and whisper tabs, plus pop-out / dockable windows for whispers and Pounce.
- Bold/Italic/Underline formatting buttons, a clickable link builder (`https://` and `furc://`), configurable dice roller (`1d20`, `1d100`, etc.), built-in spell checker/autocorrect, and `F1` hotkeys.

</details>

<details>
<summary><strong>Pounce, Notifications, and System Tray</strong></summary>

- Real-time Pounce friend watchlist with online/offline status indicators and Pounce list import.
- Saved Dreams list alongside Pounce so favorite maps stay one click away.
- Configurable notifications for Pounce alerts, whispers, and mentions, plus taskbar flashing, Linux system tray alert blinking / unread badge, and in-client Furcadia news.

</details>

<details>
<summary><strong>Logging and Cloud Folder Sync</strong></summary>

- Local chat and whisper logging with a searchable in-client log viewer and configurable retention.
- Logs default to a single **Furcadia-style shared folder** across all characters, with an optional **per-character folder** mode in Settings.
- Sync preferences and logs between computers via any folder-sync tool you already use — **Nextcloud**, **Google Drive for desktop**, **Dropbox**, or similar — without special account linking inside the client.

</details>

<details>
<summary><strong>Interface, Settings, and Cache Tools</strong></summary>

- Dedicated Settings tab with full **Colors** customization (live preview, opaque RGB pickers, cloud-synced color profiles, and one-click resets covering regular chat, `[bracket text]`, your speech, emotes, whispers/trade, emits/links, shouts, dice rolls, system/warnings/errors, typed input, active/unread/inactive tabs, and custom window/panel/accent colors), plus font/portrait sizing, themes, audio, notifications, spell check, and Windows asset import.
- **Cached Downloads** manager in Settings to inspect avatar and dream cache disk usage and flush or re-download cached avatars, dreams, or all downloads with one click if downloaded files ever need a fresh copy.
- Clean emoji-free text/ASCII UI controls across every menu, tab, and toolbar, plus automatic update checks when a newer GitHub release is published.

</details>

## Bring your Furcadia files with you

Importing is completely optional: the client works standalone on a fresh install and fetches public game assets on demand. If you already play Furcadia on Windows or Wine, the built-in importer can bring over maps, patches, portraits, settings, character details, Pounce lists, and logs from common install and Documents paths — or any folder you pick manually.

To sync settings and logs across multiple computers with Nextcloud, Google Drive for desktop, or Dropbox:

1. Create or choose a dedicated folder inside your provider's locally synced directory (for example, `Nextcloud/Furcadia Native Sync/`).
2. In Furcadia Native Client, open **Settings -> Logs**, select that folder, and click **Use Folder**.
3. Choose the same synced folder on each computer. When switching machines, let your sync provider finish syncing before launching, and restart the client after connecting if needed so it picks up the newest shared preferences.

The client manages per-machine preferences as `settings-<device>.ini` at the top level and stores logs under `logs/<device>/`, combining per-device logs automatically in the log viewer. Account email, passwords, and auto-login credentials remain only in each computer's local app configuration.

The client reads `Fox5`, `FSHX`, `FSH`, and `FR01` asset archives and supports Furcadia map formats `V01.40` and `V01.50`. Proprietary game files are not bundled in this repository; public assets are fetched from Furcadia's public servers or read from your own local installation.

## Three ways to use it

- **GUI (`furc-gui`)** — the full desktop client with dream rendering, multi-alt character tabs, whisper tabs, Trade/Gift/Cookie player card menus, Pounce, log viewer, importer, and cache management.
- **CLI (`furc-cli`)** — a terminal client for connecting, chatting, sending raw protocol commands, and watching Pounce friends from a shell or script.
- **Rust library (`furcadia_client`)** — reusable crate covering Furcadia networking, protocol parsing/encoding, authentication, map and FOX5/FSH archive decoding, palette remapping, and world state.

## Get the client

Prebuilt packages are published on the [GitHub Releases page](https://github.com/morganlowe/native-furcadia/releases). Choose the asset for your operating system:

| Platform | Release asset |
| --- | --- |
| Arch Linux, CachyOS, Manjaro, EndeavourOS | `furcadia-native-<version>-1-x86_64.pkg.tar.zst` |
| Debian, Ubuntu, Linux Mint | `furcadia-native_<version>_amd64.deb` |
| Fedora, RHEL, Rocky, AlmaLinux, openSUSE | `furcadia-native-<version>-1.<dist>.x86_64.rpm` |
| Windows 64-bit | `FurcadiaNative-Setup-<version>-x86_64.exe` |
| macOS (Apple Silicon) | `FurcadiaNative-<version>-macos-arm64.dmg` |

On Arch Linux, CachyOS, Manjaro, and EndeavourOS, install the downloaded package with:

```sh
sudo pacman -U ./furcadia-native-<version>-1-x86_64.pkg.tar.zst
```

On Debian-based distributions, install the downloaded package with:

```sh
sudo apt install ./furcadia-native_<version>_amd64.deb
```

On RPM-based distributions, install the downloaded package with:

```sh
# Fedora / RHEL / Rocky / AlmaLinux
sudo dnf install ./furcadia-native-<version>-1*.x86_64.rpm

# openSUSE
sudo zypper install --allow-unsigned-rpm ./furcadia-native-<version>-1*.x86_64.rpm
```

The Windows installer installs both `furc-gui` and `furc-cli`, creates a Start Menu shortcut, and registers an uninstaller in Windows Apps & Features.

On macOS, open the `.dmg` and drag **Furcadia Native Client** to **Applications** (`furc-cli` is also bundled inside `Contents/MacOS/`). Because the app is ad-hoc signed without an Apple Developer notarization certificate, clear the quarantine attribute before first launch if Gatekeeper blocks it:

```sh
xattr -dr com.apple.quarantine "/Applications/Furcadia Native Client.app"
```

## Build from source & build packages

You will need the [stable Rust toolchain](https://www.rust-lang.org/tools/install). Linux builds also need the system development libraries required by `eframe`/`winit` and OpenSSL (`libx11`, `libxcb`, `libxkbcommon`, OpenGL, and `openssl`/`pkg-config` dev packages).

```sh
git clone https://github.com/morganlowe/native-furcadia.git
cd native-furcadia
cargo build --release --locked --bins
```

The compiled binaries are written to `target/release/furc-gui` and `target/release/furc-cli`. Launch the graphical client with `./target/release/furc-gui`.

To build native installer packages from source:

- **Arch Linux (`makepkg`)**:
  ```sh
  sudo pacman -S --needed base-devel rust git libx11 libxcb libxkbcommon libglvnd curl bzip2 unzip openssl pkgconf
  cd packaging/arch && makepkg -si
  ```
- **Debian / Ubuntu / Linux Mint (`.deb`)**:
  ```sh
  sudo apt-get install -y build-essential dpkg-dev pkg-config libssl-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev curl bzip2 unzip
  ./packaging/debian/build-deb.sh
  ```
- **Fedora / RHEL / openSUSE (`.rpm`)**:
  ```sh
  sudo dnf install -y rpm-build gcc gcc-c++ make perl-core pkgconf-pkg-config libxcb-devel libxkbcommon-devel libX11-devel mesa-libGL-devel curl bzip2 unzip
  ./packaging/rpm/build-rpm.sh
  ```
- **Windows (Inno Setup `.exe`)** *(requires MSVC C++ Build Tools + Inno Setup 6)*:
  ```powershell
  cargo build --release --locked --bins
  & "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" packaging\windows\installer.iss
  ```
- **macOS (`.dmg`)** *(requires Xcode Command Line Tools)*:
  ```sh
  ./packaging/macos/build-dmg.sh
  ```

## Command-line client

`furc-cli` connects to the default Furcadia server using either a character name or an account email and password:

```sh
./target/release/furc-cli --name CharacterName --password 'your-password'
```

```sh
./target/release/furc-cli --account you@example.com --password 'your-password' --watch FriendOne,FriendTwo
```

While connected, type chat directly or use `/wh NAME message`, `/who`, `/onln NAME`, `/glook NAME`, `/watch NAME`, `/unwatch NAME`, and `/pounce`. Prefix a raw protocol command with `/raw ` or a backtick. Run `furc-cli --help` for options such as `--host`, `--port`, and `--timeout-secs`.

## Data and privacy

Local configuration is stored at `~/.config/furcadia-native/config.ini` on Linux/macOS (or `%APPDATA%\furcadia-native\config.ini` on Windows). Downloaded caches and logs are also stored locally. The client connects to Furcadia services for login, game sessions, news, and public assets, and to GitHub to check for newer releases; account credentials are used solely to authenticate with Furcadia.

- On Unix-like systems, `config.ini` is created atomically with owner-only (`0600`) permissions. Avoid sharing your configuration file because it may contain saved credentials.
- External asset and dream downloads are restricted to HTTP/HTTPS with size caps, and dream/portrait archives enforce strict decompression and dimension bounds.
- Because Furcadia's game server uses a legacy/expired TLS certificate, the client displays a clear connection notice in both the GUI and CLI when certificate verification is bypassed for game-server compatibility.

## Project status

Release 2.2.5 builds on the 2.2.0 automatic stock patch download/caching, legacy floor/path tile rendering, RPM, and Apple Silicon macOS `.dmg` foundation (and Milestone 2.0 stability, security, and open-source baseline) with app-owned exact panel sizes, explicit resize handles, left sidebar layout fixes, a multi-character close confirmation dialog, crisp system UI fonts & leveled tab bar alignment fixes, and 112 passing unit tests.

This is an independent, actively developed client. Some behavior and asset support may differ from the official Windows client, and compatibility can vary across dreams and custom patches. Please report reproducible bugs or feature requests on [GitHub Issues](https://github.com/morganlowe/native-furcadia/issues) with your OS and relevant log output (never attach passwords or private credentials).

## Credits and license

The desktop interface uses [egui/eframe](https://github.com/emilk/egui). The project also uses `image` for PNG decoding, `lzma-rs` for LZMA decompression, `bzip2-rs` for BZip2 data, and `openssl` for TLS connections. See [`Cargo.toml`](Cargo.toml) for full dependency and license details.

Furcadia and its protocol and assets are associated with Dragon's Eye Productions, Inc. This is an independent community client and is not affiliated with or endorsed by Dragon's Eye Productions. It does not redistribute proprietary game assets.

This software is licensed under the [Creative Commons Attribution-NonCommercial-ShareAlike 4.0 International License](LICENSE). Commercial use is not permitted under this license.

<p align="center">
  <img src="bear-built-logo.png" alt="Bear Built" width="180" />
</p>
